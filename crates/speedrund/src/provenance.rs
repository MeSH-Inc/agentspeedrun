//! Who a write says it is.
//!
//! The token a request proves decides the claimant recorded on a claim and
//! the trust that claim starts at; the agent token, held by tools that act
//! for the agent, may append only the few payload kinds the contract allows.
//! Keeping the rule here means the HTTP layer only has to say which token
//! arrived, and the service only has to check whether the run allows the
//! event.

use speedrun_core::{Claimant, EventPayload, Trust};

/// The identity a request proved by the token it carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The full token: the user, or a command the user ran.
    User,
    /// The agent token, held by the MCP server and `speedrun tool`.
    Agent,
}

/// Why a write may not be appended, before the run's own rules are consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    reason: &'static str,
}

impl Refusal {
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl Caller {
    /// The claimant recorded for a claim this caller makes.
    fn claimant(self) -> Claimant {
        match self {
            Self::User => Claimant::User,
            Self::Agent => Claimant::Agent,
        }
    }

    /// The trust a claim starts at. Evidence only raises it by verification.
    fn claim_trust(self) -> Trust {
        match self {
            Self::User => Trust::SelfDeclared,
            Self::Agent => Trust::AgentClaimed,
        }
    }
}

/// Payloads the agent token may append. Everything else is the user's to send.
fn agent_may_send(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::MilestoneProposed { .. }
            | EventPayload::MilestoneReached { .. }
            | EventPayload::MilestoneInvalidated { .. }
            | EventPayload::AgentTurnStarted { .. }
            | EventPayload::AgentTurnEnded { .. }
    )
}

/// Refuse a payload this caller may not send, and stamp a claim with the
/// claimant and trust its token implies. The claim's trust is left as sent
/// when it is already `evidence_verified`, so the service keeps refusing it.
pub fn admit(caller: Caller, payload: &mut EventPayload) -> Result<(), Refusal> {
    if matches!(caller, Caller::Agent) && !agent_may_send(payload) {
        return Err(Refusal {
            reason: "the agent token may not send this event",
        });
    }
    if let EventPayload::MilestoneReached { trust, claimant, .. } = payload {
        if *trust < Trust::EvidenceVerified {
            *trust = caller.claim_trust();
        }
        *claimant = caller.claimant();
    }
    Ok(())
}
