//! The publication pipeline (COMMONS.md §19): validate one bundle, judge it,
//! sign the judgment, append it as the next Merkle leaf, and answer with the
//! receipt. The HTTP module only maps these results onto status codes and the
//! store only reads and writes rows, so the policy lives here rather than in
//! either of them.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::anyhow;
use chrono::{SecondsFormat, Utc};
use ed25519_dalek::SigningKey;
use speedrun_core::bundle::{self, BUNDLE_FORMAT, Bundle};
use speedrun_core::commons::{Judgment, PublishReceipt, PublishedRun, TreeHead, judgment_digest};
use speedrun_core::{Event, EventPayload, RunView, event, hash, merkle, sign};
use speedrun_verify::Verifier;

use crate::judge;
use crate::log::{self, LogError};
use crate::store::{Publication, Store, StoredPublication};

/// Why a publication was refused or could not be completed.
#[derive(Debug)]
pub enum PublishError {
    /// The bundle breaks a rule; answer 400 and store nothing.
    Refused(String),
    /// The commons' own state or output is wrong; answer 500.
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for PublishError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<LogError> for PublishError {
    fn from(error: LogError) -> Self {
        match error {
            LogError::Internal(error) => Self::Internal(error),
            LogError::NotInTree | LogError::BeyondLog => {
                Self::Internal(anyhow!("the log's stored state is inconsistent"))
            }
        }
    }
}

/// A bundle that passed every ingest rule, with the run folded for judging.
struct Validated {
    events: Vec<Event>,
    view: Option<RunView>,
}

/// Validate, judge, sign and append `bundle`, whose exact request body is `body`.
pub async fn publish(
    store: &Store,
    key: &SigningKey,
    verifier: &Verifier,
    bundle: Bundle,
    body: String,
) -> Result<PublishReceipt, PublishError> {
    let digest = bundle::digest(&bundle).map_err(|error| PublishError::Internal(error.into()))?;

    // A digest already in the log is the same publication; answer it from the
    // stored judgment and leaf without validating or appending again.
    if let Some(existing) = store.publication(&digest).await? {
        return existing_receipt(store, existing).await;
    }

    let validated = validate(&bundle)?;
    let issued_at = Utc::now();
    let milestones = match &validated.view {
        // The verifier fetches over blocking HTTP; keep it off the async workers.
        Some(view) => tokio::task::block_in_place(|| judge::judge(view, &validated.events, verifier)),
        None => Vec::new(),
    };
    let mut judgment = Judgment {
        bundle_digest: digest.clone(),
        run_id: bundle.run_id.clone(),
        milestones,
        verifier_pubkey: sign::pubkey_hex(key),
        issued_at,
        signature: String::new(),
    };
    let judgment_digest_value =
        judgment_digest(&judgment).map_err(|error| PublishError::Internal(error.into()))?;
    judgment.signature = sign::sign_hash(key, &judgment_digest_value);
    let judgment_body =
        serde_json::to_string(&judgment).map_err(|error| PublishError::Internal(error.into()))?;

    let leaves = store.leaf_hashes().await?;
    let leaf_index = u64::try_from(leaves.len()).map_err(|error| PublishError::Internal(error.into()))?;
    let tree_size = leaf_index + 1;
    let leaf_hash = merkle::leaf_hash(&digest, &judgment_digest_value);
    let mut tree_leaves = leaves;
    tree_leaves.push(leaf_hash.clone());
    let inclusion = log::proof(&tree_leaves, leaf_index, Some(tree_size))?;
    let tree_head = log::signed_head(key, &tree_leaves, issued_at)?;
    let tree_head_body =
        serde_json::to_string(&tree_head).map_err(|error| PublishError::Internal(error.into()))?;

    let received_at = issued_at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    store
        .append(&Publication {
            bundle_digest: &digest,
            run_id: &bundle.run_id,
            tier: bundle.tier,
            body: &body,
            publisher_pubkey: &bundle.publisher_pubkey,
            received_at: &received_at,
            judgment: &judgment_body,
            leaf_index,
            leaf_hash: &leaf_hash,
            tree_size,
            tree_head: &tree_head_body,
        })
        .await?;

    Ok(PublishReceipt {
        bundle_digest: digest,
        judgment,
        tree_head,
        inclusion,
    })
}

/// Rebuild the public record of one stored publication.
pub fn published_run(stored: StoredPublication) -> Result<PublishedRun, PublishError> {
    let bundle: Bundle =
        serde_json::from_str(&stored.body).map_err(|error| PublishError::Internal(error.into()))?;
    let judgment: Judgment =
        serde_json::from_str(&stored.judgment).map_err(|error| PublishError::Internal(error.into()))?;
    let view = if stored.tier == 0 {
        None
    } else {
        let events = parse_events(&bundle).map_err(|error| PublishError::Internal(error.into()))?;
        let view =
            fold(&events).ok_or_else(|| PublishError::Internal(anyhow!("a stored run does not fold")))?;
        Some(view)
    };
    Ok(PublishedRun {
        bundle_digest: stored.digest,
        tier: stored.tier,
        bundle,
        judgment,
        leaf_index: stored.leaf_index,
        view,
    })
}

async fn existing_receipt(store: &Store, stored: StoredPublication) -> Result<PublishReceipt, PublishError> {
    let judgment: Judgment =
        serde_json::from_str(&stored.judgment).map_err(|error| PublishError::Internal(error.into()))?;
    let tree_head_body = store
        .latest_tree_head()
        .await?
        .ok_or_else(|| PublishError::Internal(anyhow!("the log has leaves but no tree head")))?;
    let tree_head: TreeHead =
        serde_json::from_str(&tree_head_body).map_err(|error| PublishError::Internal(error.into()))?;
    let leaves = store.leaf_hashes().await?;
    let leaf_count = u64::try_from(leaves.len()).map_err(|error| PublishError::Internal(error.into()))?;
    if leaf_count != tree_head.tree_size {
        return Err(PublishError::Internal(anyhow!(
            "the latest tree head does not match the log's leaves"
        )));
    }
    let inclusion = log::proof(&leaves, stored.leaf_index, Some(tree_head.tree_size))?;
    Ok(PublishReceipt {
        bundle_digest: stored.digest,
        judgment,
        tree_head,
        inclusion,
    })
}

fn validate(bundle: &Bundle) -> Result<Validated, PublishError> {
    if bundle.format != BUNDLE_FORMAT {
        return refused(format!("bundle format {} is not {BUNDLE_FORMAT}", bundle.format));
    }
    if bundle.tier > 2 {
        return refused(format!("tier {} is not 0, 1 or 2", bundle.tier));
    }
    let digest = bundle::digest(bundle).map_err(|error| PublishError::Internal(error.into()))?;
    if !sign::verify_hash(&bundle.publisher_pubkey, &digest, &bundle.signature) {
        return refused("the bundle signature does not verify".into());
    }
    if bundle.tier == 0 {
        validate_chain_heads(bundle)?;
        Ok(Validated {
            events: Vec::new(),
            view: None,
        })
    } else {
        validate_log(bundle)
    }
}

fn validate_chain_heads(bundle: &Bundle) -> Result<(), PublishError> {
    if !bundle.events.is_empty() || !bundle.prompts.is_empty() {
        return refused("a tier 0 bundle publishes no events or prompts".into());
    }
    if bundle.chain_heads.is_empty() {
        return refused("a tier 0 bundle must name at least one chain head".into());
    }
    let mut devices = BTreeSet::new();
    for head in &bundle.chain_heads {
        if !devices.insert(head.device_id.as_str()) {
            return refused(format!("two chain heads name device {}", head.device_id));
        }
        if !is_lower_hex_64(&head.hash) {
            return refused(format!("chain head {} is not 64 lowercase hex digits", head.hash));
        }
    }
    Ok(())
}

fn validate_log(bundle: &Bundle) -> Result<Validated, PublishError> {
    if !bundle.chain_heads.is_empty() {
        return refused("a tier 1 or 2 bundle carries no chain heads".into());
    }
    let events = parse_events(bundle)
        .map_err(|error| PublishError::Refused(format!("an event does not parse: {error}")))?;
    for event in &events {
        if event.run_id != bundle.run_id {
            return refused(format!(
                "event {} names run {}, not {}",
                event.event_id, event.run_id, bundle.run_id
            ));
        }
    }
    hash::verify_stored_chain(&bundle.events)
        .map_err(|error| PublishError::Refused(format!("the stored chains do not verify: {error}")))?;
    let signatures = sign::verify_event_signatures(&events);
    if !signatures.ok() {
        let problem = match signatures.error {
            Some(problem) => problem,
            None => "an event is unsigned".to_string(),
        };
        return refused(format!("the event signatures do not verify: {problem}"));
    }
    if !publisher_signs_a_chain(&events, &bundle.publisher_pubkey) {
        return refused("the publisher key signs none of the run's chains".into());
    }
    if events.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::RunStarted {
                project_root: Some(_),
                ..
            }
        )
    }) {
        return refused("the run's log holds a project root".into());
    }
    let view = fold(&events).ok_or_else(|| PublishError::Refused("the run does not fold".into()))?;
    if !view.is_finished() {
        return refused("the run is not finished".into());
    }
    match bundle.tier {
        1 => {
            if !bundle.prompts.is_empty() {
                return refused("a tier 1 bundle carries no prompt commitments".into());
            }
        }
        2 => check_prompts(&events, bundle)?,
        _ => return refused(format!("tier {} is not 0, 1 or 2", bundle.tier)),
    }
    Ok(Validated {
        events,
        view: Some(view),
    })
}

/// The bundle's events in run order (`event::run_order`), which is the order
/// every fold and judgment of a published run uses.
fn parse_events(bundle: &Bundle) -> Result<Vec<Event>, serde_json::Error> {
    let mut events = Vec::with_capacity(bundle.events.len());
    for body in &bundle.events {
        events.push(serde_json::from_value(body.clone())?);
    }
    Ok(event::run_order(events))
}

fn fold(events: &[Event]) -> Option<RunView> {
    let latest = events.iter().map(|event| event.occurred_at).max()?;
    RunView::fold(events, latest)
}

fn publisher_signs_a_chain(events: &[Event], publisher: &str) -> bool {
    events.iter().any(|event| match &event.payload {
        EventPayload::RunStarted {
            device_pubkey: Some(key),
            ..
        } => key.as_str() == publisher,
        EventPayload::DeviceJoined { device_pubkey: key } => key.as_str() == publisher,
        EventPayload::DeviceRotated { new_pubkey, .. } => new_pubkey.as_str() == publisher,
        _ => false,
    })
}

fn check_prompts(events: &[Event], bundle: &Bundle) -> Result<(), PublishError> {
    let mut submitted = BTreeMap::<String, usize>::new();
    for event in events {
        if matches!(&event.payload, EventPayload::PromptSubmitted { .. }) {
            *submitted.entry(event.event_id.clone()).or_insert(0) += 1;
        }
    }
    let mut committed = BTreeMap::<String, usize>::new();
    for prompt in &bundle.prompts {
        *committed.entry(prompt.event_id.clone()).or_insert(0) += 1;
    }
    if submitted != committed {
        return refused("tier 2 must name each prompt_submitted event exactly once".into());
    }
    Ok(())
}

pub(crate) fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn refused<T>(message: String) -> Result<T, PublishError> {
    Err(PublishError::Refused(message))
}
