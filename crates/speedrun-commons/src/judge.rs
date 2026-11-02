//! Judgment of a published run's claims (COMMONS.md §6, §19). Nothing here
//! trusts the client's verdicts: a claim is `evidence_verified` only when the
//! shared verifier, run here, passes the claim's evidence.

use chrono::{DateTime, Utc};
use speedrun_core::commons::MilestoneJudgment;
use speedrun_core::{Event, EventPayload, Evidence, RunView, Trust};
use speedrun_verify::{Job, Verifier};

/// Judge every milestone of `run`, the fold of `events`, in the run's
/// milestone order.
pub fn judge(run: &RunView, events: &[Event], verifier: &Verifier) -> Vec<MilestoneJudgment> {
    let mut deployed_url: Option<String> = None;
    let mut judgments = Vec::with_capacity(run.milestones.len());
    for milestone in &run.milestones {
        let claim = current_claim(events, &milestone.milestone_id);
        let (claimed, granted, problems) = match claim.as_ref() {
            None => (None, None, Vec::new()),
            Some(claim) => {
                let claimed = milestone.trust;
                if Job::has_checkable_evidence(&claim.evidence) {
                    let job = Job {
                        run_id: run.run_id.clone(),
                        cause_event_id: claim.event_id.clone(),
                        milestone_id: milestone.milestone_id.clone(),
                        key: milestone.key.clone(),
                        evidence: claim.evidence.clone(),
                        project_root: None,
                        run_started_at: run.started_at,
                        claimed_at: claim.occurred_at,
                        deployed_url: deployed_url.clone(),
                    };
                    let outcome = verifier.verify(&job);
                    if outcome.verified() {
                        (claimed, Some(Trust::EvidenceVerified), Vec::new())
                    } else {
                        (claimed, claimed.map(capped), outcome.problems)
                    }
                } else {
                    (claimed, claimed.map(capped), Vec::new())
                }
            }
        };
        if milestone.key.as_deref() == Some("deployed") && granted == Some(Trust::EvidenceVerified) {
            deployed_url = claim.as_ref().and_then(evidence_url);
        }
        judgments.push(MilestoneJudgment {
            milestone_id: milestone.milestone_id.clone(),
            claimed,
            granted,
            problems,
        });
    }
    judgments
}

fn capped(trust: Trust) -> Trust {
    if trust > Trust::AgentClaimed {
        Trust::AgentClaimed
    } else {
        trust
    }
}

struct Claim {
    event_id: String,
    occurred_at: DateTime<Utc>,
    evidence: Vec<Evidence>,
}

/// The last `MilestoneReached` for `milestone_id` since its last invalidation.
fn current_claim(events: &[Event], milestone_id: &str) -> Option<Claim> {
    let mut current = None;
    for event in events {
        match &event.payload {
            EventPayload::MilestoneReached {
                milestone_id: id,
                evidence,
                ..
            } if id.as_str() == milestone_id => {
                current = Some(Claim {
                    event_id: event.event_id.clone(),
                    occurred_at: event.occurred_at,
                    evidence: evidence.clone(),
                });
            }
            EventPayload::MilestoneInvalidated { milestone_id: id, .. } if id.as_str() == milestone_id => {
                current = None;
            }
            _ => {}
        }
    }
    current
}

fn evidence_url(claim: &Claim) -> Option<String> {
    claim.evidence.iter().find_map(|evidence| match evidence {
        Evidence::Url { url } => Some(url.clone()),
        _ => None,
    })
}
