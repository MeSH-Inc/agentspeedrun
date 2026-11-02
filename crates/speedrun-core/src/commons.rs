//! The commons server's protocol (COMMONS.md §7, §8): what it answers to a
//! publication and to readers. Its signatures are `sign::sign_hash` by the
//! commons key over each object's digest, so they check with
//! `sign::verify_hash` and the key from `/v1/health`.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bundle::Bundle;
use crate::hash::canonical_digest;
use crate::{RunView, Trust};

/// `GET /v1/health` on the commons.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CommonsHealth {
    pub ok: bool,
    pub version: String,
    /// Hex ed25519 public key that signs judgments and tree heads.
    pub commons_pubkey: String,
    /// Leaves in the log.
    pub tree_size: u64,
}

/// The commons' verdict on one milestone of a published run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MilestoneJudgment {
    pub milestone_id: String,
    /// The strongest trust the bundle's log records for the milestone.
    pub claimed: Option<Trust>,
    /// The trust the commons grants: `evidence_verified` only when its own
    /// verification of the claim's evidence passed, otherwise the claimed
    /// trust capped at `agent_claimed`. `None` when nothing was claimed.
    pub granted: Option<Trust>,
    /// Why the commons' verification failed. Empty when it passed or when the
    /// claim had no checkable evidence.
    pub problems: Vec<String>,
}

/// A signed judgment of one bundle. Its milestones follow the run's
/// milestone order; a tier 0 bundle has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Judgment {
    pub bundle_digest: String,
    pub run_id: String,
    pub milestones: Vec<MilestoneJudgment>,
    pub verifier_pubkey: String,
    pub issued_at: DateTime<Utc>,
    pub signature: String,
}

/// A signed tree head: the log's size and root at an instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TreeHead {
    pub tree_size: u64,
    pub root_hash: String,
    pub issued_at: DateTime<Utc>,
    pub signature: String,
}

/// An inclusion proof of one leaf against a tree of `tree_size` leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InclusionProof {
    pub leaf_index: u64,
    pub tree_size: u64,
    pub leaf_hash: String,
    pub audit_path: Vec<String>,
}

/// `POST /v1/bundles`: the bundle was stored, judged and anchored.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PublishReceipt {
    pub bundle_digest: String,
    pub judgment: Judgment,
    pub tree_head: TreeHead,
    pub inclusion: InclusionProof,
}

/// `GET /v1/runs/{run_id}`: everything public about a run's latest publication.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PublishedRun {
    pub bundle_digest: String,
    pub tier: u8,
    pub bundle: Bundle,
    pub judgment: Judgment,
    pub leaf_index: u64,
    /// The run folded from the bundle's events; `None` at tier 0.
    pub view: Option<RunView>,
}

// --- Persons and links (COMMONS.md §3, §20) -------------------------------------------------

/// The version of [`LinkClaim`] this build writes and the commons accepts.
pub const LINK_FORMAT: u32 = 1;

/// A person, as the GitHub account the commons saw authorize a link. `github_id`
/// is the identity; `login` is the name it had then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Person {
    pub github_id: u64,
    pub login: String,
}

/// `POST /v1/links`: a device asks to be linked to whoever authorizes at GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LinkStart {
    /// Hex ed25519 public key of the device to link.
    pub device_pubkey: String,
}

/// The answer to [`LinkStart`]: what the person enters at GitHub, and how
/// often the device may poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LinkStarted {
    pub link_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: u64,
    pub expires_at: DateTime<Utc>,
}

/// `POST /v1/links/{link_id}/poll` while the link can still complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LinkState {
    /// GitHub has not seen the person authorize yet (HTTP 202).
    Pending,
    /// GitHub named this person (HTTP 200).
    Authorized { person: Person },
}

/// What a device signs to link itself to a person at one commons. Both the
/// device and the commons sign [`link_claim_digest`], so neither can invent a
/// link alone (COMMONS.md §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LinkClaim {
    pub format: u32,
    pub link_id: String,
    pub device_pubkey: String,
    pub github_id: u64,
    pub login: String,
    /// The commons this claim is for, so it cannot be replayed at another.
    pub commons_pubkey: String,
    pub issued_at: DateTime<Utc>,
}

/// `GET /v1/device` on the daemon: the key that signs this device's chains now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeviceKey {
    pub device_pubkey: String,
    /// `sign::fingerprint` of `device_pubkey`, the events' `device_id`.
    pub device_id: String,
}

/// The daemon's answer to a link claim it agreed to sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DeviceSignature {
    pub device_signature: String,
}

/// `POST /v1/links/{link_id}/confirm`: the claim and the device's signature of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LinkConfirm {
    pub claim: LinkClaim,
    pub device_signature: String,
}

/// A link both sides signed: `device_signature` by `claim.device_pubkey`,
/// `commons_signature` by `claim.commons_pubkey`, both over
/// [`link_claim_digest`] of `claim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Link {
    pub claim: LinkClaim,
    pub device_signature: String,
    pub commons_signature: String,
}

/// One publication, as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublicationSummary {
    pub run_id: String,
    pub bundle_digest: String,
    pub tier: u8,
    pub received_at: DateTime<Utc>,
    pub leaf_index: u64,
}

/// `GET /v1/people/{login}`: a person, every link they made, and the
/// publications signed by a linked device, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PersonRecord {
    pub person: Person,
    pub links: Vec<Link>,
    pub publications: Vec<PublicationSummary>,
}

/// The digest both signatures of a [`Link`] cover: the claim's canonical JSON.
pub fn link_claim_digest(claim: &LinkClaim) -> Result<String, serde_json::Error> {
    Ok(canonical_digest(&serde_json::to_value(claim)?))
}

/// The digest a judgment's signature covers: its canonical JSON without `signature`.
pub fn judgment_digest(judgment: &Judgment) -> Result<String, serde_json::Error> {
    Ok(unsigned_digest(serde_json::to_value(judgment)?))
}

/// The digest a tree head's signature covers: its canonical JSON without `signature`.
pub fn tree_head_digest(head: &TreeHead) -> Result<String, serde_json::Error> {
    Ok(unsigned_digest(serde_json::to_value(head)?))
}

fn unsigned_digest(mut value: serde_json::Value) -> String {
    if let serde_json::Value::Object(fields) = &mut value {
        fields.remove("signature");
    }
    canonical_digest(&value)
}
