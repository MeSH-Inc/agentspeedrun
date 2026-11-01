use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bump when the envelope or a payload changes incompatibly.
pub const SCHEMA_VERSION: u32 = 1;

/// A single immutable fact in a run's log, as stored by the daemon.
///
/// `seq`, `recorded_at`, `device_id`, `prev_hash` and `hash` are assigned by
/// the daemon at append time; clients only supply `event_id` (their
/// idempotency key), `occurred_at` and `payload`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    pub event_id: String,
    pub schema_version: u32,
    pub run_id: String,
    /// Position in the run's log, starting at 1. Gapless.
    pub seq: u64,
    pub device_id: String,
    /// When the fact happened according to the client clock.
    pub occurred_at: DateTime<Utc>,
    /// When the daemon persisted it.
    pub recorded_at: DateTime<Utc>,
    pub payload: EventPayload,
    /// Hash of the previous event in this run; `None` only for `seq == 1`.
    pub prev_hash: Option<String>,
    /// sha256 over the canonical JSON of this event without `hash` or `sig`.
    pub hash: String,
    /// Ed25519 signature over the `hash` (hex), from the device's key.
    /// Absent in schema v1 events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    RunStarted {
        project_id: String,
        project_root: Option<String>,
        category_id: String,
        category_version: u32,
        /// Hex ed25519 public key of the device that signs this chain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_pubkey: Option<String>,
    },
    PromptSubmitted {
        session_id: String,
        /// Which tool the prompt came from, e.g. `claude-code`, `opencode`.
        host: String,
        model: Option<String>,
        content: String,
        /// True when secret redaction changed `content`.
        redacted: bool,
    },
    AgentTurnStarted {
        session_id: String,
    },
    AgentTurnEnded {
        session_id: String,
    },
    MilestoneProposed {
        milestone_id: String,
        origin: MilestoneOrigin,
        /// Stable key for category milestones, e.g. `happy-path-demo`.
        key: Option<String>,
        title: String,
        description: Option<String>,
        required_trust: Trust,
    },
    MilestoneAccepted {
        milestone_id: String,
    },
    MilestoneRejected {
        milestone_id: String,
        reason: Option<String>,
    },
    MilestoneReached {
        milestone_id: String,
        trust: Trust,
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
    MilestoneVerified {
        milestone_id: String,
        trust: Trust,
        verifier: String,
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
    MilestoneInvalidated {
        milestone_id: String,
        reason: String,
    },
    RunPaused {
        reason: Option<String>,
    },
    RunResumed {},
    RunFinished {
        outcome: RunOutcome,
        reason: Option<String>,
    },
}

impl EventPayload {
    /// The `type` tag, as stored in the `events.type` column.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RunStarted { .. } => "run_started",
            Self::PromptSubmitted { .. } => "prompt_submitted",
            Self::AgentTurnStarted { .. } => "agent_turn_started",
            Self::AgentTurnEnded { .. } => "agent_turn_ended",
            Self::MilestoneProposed { .. } => "milestone_proposed",
            Self::MilestoneAccepted { .. } => "milestone_accepted",
            Self::MilestoneRejected { .. } => "milestone_rejected",
            Self::MilestoneReached { .. } => "milestone_reached",
            Self::MilestoneVerified { .. } => "milestone_verified",
            Self::MilestoneInvalidated { .. } => "milestone_invalidated",
            Self::RunPaused { .. } => "run_paused",
            Self::RunResumed { .. } => "run_resumed",
            Self::RunFinished { .. } => "run_finished",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneOrigin {
    /// Comes from the run's category ruleset.
    Predefined,
    /// Entered by the user.
    User,
    /// Proposed by an agent; must be accepted by the user before it counts.
    AgentGenerated,
}

/// How much a milestone claim can be believed. Ordered weakest → strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    SelfDeclared,
    AgentClaimed,
    EvidenceVerified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    GitCommit { sha: String },
    Url { url: String },
    CiRun { url: String },
    StripeEvent { event_id: String },
    Note { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// Every category milestone counts.
    Completed,
    /// The user reset; the next prompt starts a fresh run.
    Abandoned,
}
