//! Request/response bodies for the daemon's local HTTP API (`/v1`).

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Category, Event, EventPayload, RunView, live::LiveSnapshot};

/// Append one event to an existing run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppendRequest {
    /// Client idempotency key. Retrying with the same id is a no-op.
    pub event_id: String,
    pub run_id: String,
    pub occurred_at: Option<DateTime<Utc>>,
    pub payload: EventPayload,
}

/// Append one event to whichever run is active for a project. Lets clients
/// write (and spool while offline) without knowing the run id.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectAppendRequest {
    pub event_id: String,
    pub occurred_at: Option<DateTime<Utc>>,
    pub payload: EventPayload,
}

/// Record a prompt for a project, starting a run if none is active.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptRequest {
    pub event_id: String,
    pub project_id: String,
    pub project_root: Option<String>,
    /// Category for a run started by this prompt.
    pub category_id: String,
    /// The category version the project pinned (`category_version` in
    /// `.speedrun/project.json`). `None` takes the daemon's latest; a version
    /// the daemon does not know is a 400 and starts no run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_version: Option<u32>,
    pub session_id: String,
    pub host: String,
    pub model: Option<String>,
    pub content: String,
    pub occurred_at: Option<DateTime<Utc>>,
    /// Also mark an agent turn as started for this session.
    #[serde(default = "yes")]
    pub starts_turn: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PromptResponse {
    pub run_id: String,
    /// True when this prompt started a new run.
    pub run_started: bool,
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppendResponse {
    pub event: Event,
    /// False when the event_id already existed.
    pub inserted: bool,
    /// Events the daemon appended as a consequence (verification, completion).
    #[serde(default)]
    pub derived: Vec<Event>,
    /// Result of checking attached evidence, when there was any to check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationReport>,
}

/// What the daemon's verifiers concluded about a claim's evidence. A failed
/// check is not recorded in the log: the claim keeps the trust it was made
/// with, and claiming again re-runs the checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationReport {
    pub verified: bool,
    /// Verifiers that passed, joined with `+` (e.g. `git+url`), when verified.
    pub verifier: Option<String>,
    /// Why it was not verified.
    #[serde(default)]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ActiveRunResponse {
    pub run: Option<RunView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CategoriesResponse {
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChainReport {
    pub run_id: String,
    pub events: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub head_hash: Option<String>,
    /// Events carrying a verified device signature.
    #[serde(default)]
    pub signed: u64,
    /// Events with no signature: schema v1 logs, or a chain that predates
    /// signing on this device.
    #[serde(default)]
    pub unsigned: u64,
}

/// Version of the plugin ↔ daemon protocol. A client compares it with its
/// own before sending; the binary version alone does not say what changed.
pub const PROTOCOL_VERSION: u32 = 1;

/// `GET /v1/health[?nonce=<hex>]`. Never carries a token. With a nonce, the
/// daemon proves it holds this data dir's token: `proof` is
/// `digest::token_proof(token, nonce)`, which a client checks before sending
/// a body, so a daemon serving another data dir is recognised, not trusted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HealthResponse {
    pub ok: bool,
    /// The daemon's binary version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// [`PROTOCOL_VERSION`] of the daemon.
    pub protocol_version: u32,
    /// Hex SHA-256 of the canonical path of the data dir the daemon serves.
    pub data_dir_id: String,
    /// The daemon's process id, so `speedrun daemon stop` can wait for its exit.
    pub pid: u32,
    /// Present exactly when the request carried a nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorBody {
    pub error: String,
}

/// Everything that crosses the plugin ↔ daemon boundary, for schema export.
#[derive(JsonSchema)]
#[allow(dead_code)]
pub struct Protocol {
    event: Event,
    append_request: AppendRequest,
    project_append_request: ProjectAppendRequest,
    append_response: AppendResponse,
    prompt_request: PromptRequest,
    prompt_response: PromptResponse,
    active_run_response: ActiveRunResponse,
    categories_response: CategoriesResponse,
    chain_report: ChainReport,
    export_response: ExportResponse,
    rotate_response: RotateResponse,
    health_response: HealthResponse,
    live_snapshot: LiveSnapshot,
    error_body: ErrorBody,
}

/// JSON Schema for the whole protocol, committed as `schema/protocol.schema.json`.
pub fn protocol_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Protocol)).expect("schema serializes")
}

#[cfg(test)]
mod tests {
    #[test]
    fn committed_schema_is_current() {
        let committed: serde_json::Value =
            serde_json::from_str(include_str!("../../../schema/protocol.schema.json")).unwrap();
        assert!(
            committed == super::protocol_schema(),
            "schema/protocol.schema.json is stale; run `cargo run -q --bin speedrun -- schema > schema/protocol.schema.json`"
        );
    }
}

/// `GET /v1/runs/{run_id}/export?tier=2`: what a tier 2 publication would
/// carry about the run's prompts. No prompt text.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExportResponse {
    pub run_id: String,
    pub tier: u8,
    /// sha256 (hex) of the run's disclosure key, as in `RunStarted`.
    pub disclosure_commitment: String,
    /// One entry per prompt, in log order.
    pub prompts: Vec<PromptCommitment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptCommitment {
    pub event_id: String,
    /// HMAC-SHA256 (hex) of the redacted prompt text under the run's
    /// disclosure key.
    pub commitment: String,
    /// Length of the redacted text, in characters.
    pub length: u64,
    pub redacted: bool,
}

/// `POST /v1/device/rotate`: the device's signing key was replaced.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RotateResponse {
    /// The key that signs from now on (hex).
    pub device_pubkey: String,
    /// The key it replaced (hex).
    pub previous_pubkey: String,
}
