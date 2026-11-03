use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bump when the envelope or a payload changes incompatibly.
pub const SCHEMA_VERSION: u32 = 3;

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

/// A run's events in the one order a published run is folded and judged in:
/// each device's chain in `seq` order, the chains merged by `recorded_at`, a
/// tie going to the smaller device id. A single-device run comes out in `seq`
/// order. The events alone fix this order, so anyone holding a bundle folds the
/// view the commons folded (COMMONS.md §19).
pub fn run_order(mut events: Vec<Event>) -> Vec<Event> {
    events.sort_by(|a, b| (a.device_id.as_str(), a.seq).cmp(&(b.device_id.as_str(), b.seq)));
    let mut chains: BTreeMap<String, VecDeque<Event>> = BTreeMap::new();
    let count = events.len();
    for event in events {
        chains
            .entry(event.device_id.clone())
            .or_default()
            .push_back(event);
    }
    let mut ordered = Vec::with_capacity(count);
    loop {
        // BTreeMap iterates in device-id order, so the first minimum wins a tie.
        let next = chains
            .iter()
            .filter_map(|(device, chain)| chain.front().map(|event| (event.recorded_at, device)))
            .min()
            .map(|(_, device)| device.clone());
        let Some(device) = next else {
            return ordered;
        };
        if let Some(event) = chains.get_mut(&device).and_then(VecDeque::pop_front) {
            ordered.push(event);
        }
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use chrono::TimeZone;

    fn event(device: &str, seq: u64, recorded_minute: u32) -> Event {
        let at = Utc.with_ymd_and_hms(2026, 11, 2, 12, recorded_minute, 0).unwrap();
        Event {
            event_id: format!("{device}-{seq}"),
            schema_version: SCHEMA_VERSION,
            run_id: "r".into(),
            seq,
            device_id: device.into(),
            occurred_at: at,
            recorded_at: at,
            payload: EventPayload::RunResumed {},
            prev_hash: None,
            hash: String::new(),
            sig: None,
        }
    }

    fn ids(events: &[Event]) -> Vec<&str> {
        events.iter().map(|e| e.event_id.as_str()).collect()
    }

    #[test]
    fn chains_merge_by_recorded_time_and_keep_their_own_order() {
        // b's clock runs behind at seq 2, which must not lift it above b-1.
        let shuffled = vec![
            event("b", 2, 1),
            event("a", 2, 4),
            event("b", 1, 3),
            event("a", 1, 0),
            event("b", 3, 4),
        ];
        assert_eq!(ids(&run_order(shuffled)), ["a-1", "b-1", "b-2", "a-2", "b-3"]);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    RunStarted {
        project_id: String,
        /// A local path, so never written by this build: the daemon keeps a
        /// run's root outside the log, which is published verbatim. Read only
        /// so that older logs still load; a run whose log holds one cannot be
        /// published.
        #[serde(default, skip_serializing_if = "Option::is_none")]
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
        /// key, which never leaves the machine. Empty means metadata-only
        /// capture: no text was kept or committed.
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
        /// Who made the claim. Set by the daemon from the token the request
        /// carried; a value sent by the client is overwritten.
        #[serde(default)]
        claimant: Claimant,
    },
    /// The user confirms an agent's claim on a milestone whose rules require
    /// the user's word (`required_trust` `self_declared`). Only the full
    /// token may send it.
    MilestoneConfirmed {
        milestone_id: String,
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
            Self::MilestoneConfirmed { .. } => "milestone_confirmed",
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

/// Who claimed a milestone, by the token the claim arrived with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Claimant {
    /// The full token: the user, or a command the user ran.
    #[default]
    User,
    /// The agent token (`$DATA_DIR/agent-token`), held by agent tools.
    Agent,
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
    LocalTest { observation_id: String },
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
