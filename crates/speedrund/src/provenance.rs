//! Who a write says it is.
//!
//! The token a request proves decides the claimant recorded on a claim and
//! the trust that claim starts at; the agent token, held by tools that act
//! for the agent, may append only the few payload kinds the contract allows.
//! Keeping the rule here means the HTTP layer only has to say which token
//! arrived, and the service only has to check whether the run allows the
//! event.

use speedrun_core::{Claimant, EventPayload, Trust};

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

/// The trust a claim starts at. Evidence only raises it by verification.
fn claim_trust(claimant: Claimant) -> Trust {
    match claimant {
        Claimant::User => Trust::SelfDeclared,
        Claimant::Agent => Trust::AgentClaimed,
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

/// Refuse a payload this claimant may not send, and stamp a claim with the
/// claimant and trust its token implies. The claim's trust is left as sent
/// when it is already `evidence_verified`, so the service keeps refusing it.
pub fn admit(claimant: Claimant, payload: &mut EventPayload) -> Result<(), Refusal> {
    if claimant == Claimant::Agent && !agent_may_send(payload) {
        return Err(Refusal {
            reason: "the agent token may not send this event",
        });
    }
    if let EventPayload::MilestoneReached {
        trust,
        claimant: recorded,
        ..
    } = payload
    {
        if *trust < Trust::EvidenceVerified {
            *trust = claim_trust(claimant);
        }
        *recorded = claimant;
    }
    Ok(())
}
