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
