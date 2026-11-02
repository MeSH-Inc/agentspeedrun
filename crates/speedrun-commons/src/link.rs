//! Linking a device key to a GitHub person (COMMONS.md §20). The device flow's
//! wire format belongs to `github`; the rows belong to `store`; this module
//! owns the policy in between: what a pending link may still do, what a
//! confirmation must prove, and when a link can never complete.

use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use speedrun_core::commons::{
    LINK_FORMAT, Link, LinkConfirm, LinkStart, LinkStarted, LinkState, PersonRecord, link_claim_digest,
};
use speedrun_core::sign;

use crate::github::{self, GitHubConfig, GitHubError};
use crate::record::is_lower_hex_64;
use crate::store::{NewPendingLink, PendingState, Store};

/// Why a link request could not be answered.
#[derive(Debug)]
pub enum LinkError {
    /// The request is not a well-formed link request; answer 400.
    BadRequest(String),
    /// No pending link (and no confirmed link) has that id; answer 404.
    NotFound,
    /// The link has not been authorized yet; answer 409.
    Conflict,
    /// The link expired or was refused; answer 410.
    Gone,
    /// No OAuth app is configured; answer 503.
    Unavailable,
    /// GitHub could not be reached or answered unusably; answer 502.
    GitHub(GitHubError),
    /// The commons' own state is wrong; answer 500.
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for LinkError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<GitHubError> for LinkError {
    fn from(error: GitHubError) -> Self {
        Self::GitHub(error)
    }
}

/// Start a pending link for `start.device_pubkey`, asking GitHub for the code
/// the person enters.
pub async fn start(store: &Store, github: &GitHubConfig, start: LinkStart) -> Result<LinkStarted, LinkError> {
    let client_id = match github.client_id.as_deref() {
        Some(client_id) => client_id,
        None => return Err(LinkError::Unavailable),
    };
    if !is_lower_hex_64(&start.device_pubkey) {
        return Err(LinkError::BadRequest(
            "device_pubkey must be 64 lowercase hex digits".into(),
        ));
    }
    let request = || github::request_device_code(github, client_id);
    let code = tokio::task::block_in_place(request)?;
    let lifetime = i64::try_from(code.expires_in).map_err(|error| LinkError::Internal(error.into()))?;
    let expires_at = Utc::now() + Duration::seconds(lifetime);
    let link_id = ulid::Ulid::new().to_string();
    store
        .insert_pending_link(&NewPendingLink {
            link_id: &link_id,
            device_pubkey: &start.device_pubkey,
            device_code: &code.device_code,
            expires_at,
        })
        .await?;
    Ok(LinkStarted {
        link_id,
        user_code: code.user_code,
        verification_uri: code.verification_uri,
        interval_seconds: code.interval_seconds,
        expires_at,
    })
}

/// Poll a pending link once. A link already authorized answers from the
/// stored person, without asking GitHub again.
pub async fn poll(store: &Store, github: &GitHubConfig, link_id: &str) -> Result<LinkState, LinkError> {
    let pending = match store.pending_link(link_id).await? {
        Some(pending) => pending,
        None => return Err(LinkError::NotFound),
    };
    if pending.state == PendingState::Refused {
        return Err(LinkError::Gone);
    }
    if Utc::now() >= pending.expires_at {
        return Err(LinkError::Gone);
    }
    if let Some(person) = pending.person {
        return Ok(LinkState::Authorized { person });
    }
    let client_id = match github.client_id.as_deref() {
        Some(client_id) => client_id,
        None => return Err(LinkError::Unavailable),
    };
    let poll = || github::poll_token(github, client_id, &pending.device_code);
    let answer = tokio::task::block_in_place(poll)?;
    match answer {
        github::TokenPoll::Pending | github::TokenPoll::SlowDown => Ok(LinkState::Pending),
        github::TokenPoll::Refused => {
            store.refuse_pending_link(link_id).await?;
            Err(LinkError::Gone)
        }
        github::TokenPoll::Token(token) => {
            let fetch = || github::fetch_user(github, &token);
            let person = tokio::task::block_in_place(fetch)?;
            store.authorize_pending_link(link_id, &person, Utc::now()).await?;
            Ok(LinkState::Authorized { person })
        }
    }
}

/// Check a device's claim, countersign the same digest, and store the link.
pub async fn confirm(
    store: &Store,
    key: &SigningKey,
    link_id: &str,
    confirm: LinkConfirm,
) -> Result<Link, LinkError> {
    if let Some(link) = store.stored_link(link_id).await? {
        return Ok(link);
    }
    let pending = match store.pending_link(link_id).await? {
        Some(pending) => pending,
        None => return Err(LinkError::NotFound),
    };
    if pending.state == PendingState::Refused {
        return Err(LinkError::Gone);
    }
    if Utc::now() >= pending.expires_at {
        return Err(LinkError::Gone);
    }
    let person = match pending.person {
        Some(person) => person,
        None => return Err(LinkError::Conflict),
    };
    let claim = confirm.claim;
    if claim.format != LINK_FORMAT {
        return Err(LinkError::BadRequest(format!(
            "claim format {} is not {LINK_FORMAT}",
            claim.format
        )));
    }
    if claim.link_id != link_id {
        return Err(LinkError::BadRequest("the claim names another link".into()));
    }
    if claim.device_pubkey != pending.device_pubkey {
        return Err(LinkError::BadRequest("the claim names another device key".into()));
    }
    if claim.github_id != person.github_id || claim.login != person.login {
        return Err(LinkError::BadRequest("the claim names another person".into()));
    }
    if claim.commons_pubkey != sign::pubkey_hex(key) {
        return Err(LinkError::BadRequest("the claim names another commons".into()));
    }
    let digest = link_claim_digest(&claim).map_err(|error| LinkError::Internal(error.into()))?;
    if !sign::verify_hash(&claim.device_pubkey, &digest, &confirm.device_signature) {
        return Err(LinkError::BadRequest("device_signature does not verify".into()));
    }
    let link = Link {
        claim,
        device_signature: confirm.device_signature,
        commons_signature: sign::sign_hash(key, &digest),
    };
    let body = serde_json::to_string(&link).map_err(|error| LinkError::Internal(error.into()))?;
    store.insert_link(&link, &body, Utc::now()).await?;
    Ok(link)
}

/// A person by their current login, with their links and publications.
pub async fn person(store: &Store, login: &str) -> Result<PersonRecord, LinkError> {
    match store.person_record(login).await? {
        Some(record) => Ok(record),
        None => Err(LinkError::NotFound),
    }
}
