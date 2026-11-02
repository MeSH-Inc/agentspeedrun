use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bump when the envelope or a payload changes incompatibly.
pub const SCHEMA_VERSION: u32 = 2;

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
    /// Ed25519 signature over the `hash` (hex), from the key that signs this
    /// device's chain at this point. Required: an unsigned event fails verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// A run's events split into one chain per device, as `device_id` names them.
///
/// Each chain keeps the order its events arrived in; callers pass a run's
/// events with every chain already in `seq` order. Chains themselves are
/// ordered by device id and are otherwise independent: a multi-device log is
/// several chains interleaved, not one global sequence.
pub(crate) fn device_chains(events: &[Event]) -> Vec<Vec<&Event>> {
    let mut chains: BTreeMap<&str, Vec<&Event>> = BTreeMap::new();
    for event in events {
        chains.entry(event.device_id.as_str()).or_default().push(event);
    }
    chains.into_values().collect()
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
        /// The project's git HEAD when the run started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_head: Option<String>,
        /// Committer time of `start_head`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start_head_committed_at: Option<DateTime<Utc>>,
        /// sha256 (hex) of the canonical JSON of the category the run pinned.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ruleset_digest: Option<String>,
        /// sha256 (hex) of the run's disclosure key, which stays on the device
        /// until the run is revealed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disclosure_commitment: Option<String>,
    },
    /// A prompt, without its text. The text is kept outside the chain, keyed
    /// by `event_id`, so deleting it never breaks the log.
    PromptSubmitted {
        session_id: String,
        /// Which tool the prompt came from, e.g. `claude-code`, `opencode`.
        host: String,
        model: Option<String>,
        /// HMAC-SHA256 (hex) of the redacted text under the device's prompt
        /// key, which never leaves the machine.
        content_digest: String,
        /// Length of the redacted text, in characters.
        length: u64,
        /// True when secret redaction changed the text.
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
        /// Replaces the trust the proposal required, raising or lowering it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        required_trust: Option<Trust>,
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
    /// The first event of a device's chain in a run it did not start: names
    /// the key that signs that chain.
    DeviceJoined {
        device_pubkey: String,
    },
    /// The device's signing key changed. Signed by the new key; `certificate`
    /// is the old key's signature over the new public key, so the chain
    /// stays verifiable from public keys alone.
    DeviceRotated {
        new_pubkey: String,
        certificate: String,
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
            Self::DeviceJoined { .. } => "device_joined",
            Self::DeviceRotated { .. } => "device_rotated",
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
