//! Request/response bodies for the daemon's local HTTP API (`/v1`).

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Category, Event, EventPayload, RunView};

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
