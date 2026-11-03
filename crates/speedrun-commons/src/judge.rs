//! Judgment of a published run's claims (COMMONS.md §6, §19). Nothing here
//! trusts the client's verdicts: a claim is `evidence_verified` only when the
//! shared verifier, run here, passes the claim's evidence.

use chrono::{DateTime, Utc};
use speedrun_core::category::{self, Category};
use speedrun_core::commons::{MilestoneJudgment, PublicPolicyStatus};
use speedrun_core::event::MilestoneOrigin;
use speedrun_core::{Event, EventPayload, Evidence, RunView, Trust};
use speedrun_verify::{Job, Net, NoSecrets, Verifier};

/// The verifier the commons judges with: the network, and no secrets (§19).
pub fn verifier() -> Verifier {
    Verifier {
        fetch: Box::new(Net::new()),
        secrets: Box::new(NoSecrets),
    }
}

/// Judge every milestone of `run`, the fold of `events`, in the run's
/// milestone order.
pub fn judge(run: &RunView, events: &[Event], verifier: &Verifier) -> Vec<MilestoneJudgment> {
    judge_policy(run, events, verifier).0
}

pub fn judge_policy(
    run: &RunView,
    events: &[Event],
    verifier: &Verifier,
) -> (Vec<MilestoneJudgment>, PublicPolicyStatus) {
    let categories = category::builtin();
    let policy = categories
        .as_ref()
        .ok()
        .and_then(|categories| pinned_category(run, events, categories).ok());
    let unavailable = categories
        .as_ref()
        .map_err(|_| "built-in rulesets cannot be read".to_string())
        .and_then(|categories| pinned_category(run, events, categories).map(|_| ()));
    let mut deployed_url: Option<String> = None;
    let mut judgments = Vec::with_capacity(run.milestones.len());
    for milestone in &run.milestones {
        let claim = current_claim(events, &milestone.milestone_id);
        let mut verification = None;
        let (claimed, granted, problems) = match claim.as_ref() {
            None => (None, None, Vec::new()),
            Some(claim) => {
                let claimed = milestone.trust;
                if Job::has_checkable_evidence(&claim.evidence) {
                    let job = Job {
                        scope: speedrun_core::api::VerificationScope::Public,
                        run_id: run.run_id.clone(),
                        cause_event_id: claim.event_id.clone(),
                        milestone_id: milestone.milestone_id.clone(),
                        key: None,
                        policy: policy.and_then(|category| {
                            category
                                .milestones
                                .iter()
                                .find(|template| Some(&template.key) == milestone.key.as_ref())
                                .and_then(|template| template.verification_policy)
                        }),
                        local_test_verified: false,
                        evidence: claim.evidence.clone(),
                        project_root: None,
                        run_started_at: run.started_at,
                        claimed_at: claim.occurred_at,
                        deployed_url: deployed_url.clone(),
                    };
                    let outcome = verifier.verify(&job);
                    verification = Some(outcome.report());
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
            verification,
        });
    }
    let status = match (unavailable, policy) {
        (Err(reason), _) => PublicPolicyStatus::Unavailable { reason },
        (_, Some(category)) => {
            let mut unavailable = false;
            let mut complete = true;
            for template in &category.milestones {
                let Some((milestone, judgment)) =
                    run.milestones.iter().zip(&judgments).find(|(milestone, _)| {
                        milestone.origin == MilestoneOrigin::Predefined
                            && milestone.key.as_ref() == Some(&template.key)
                    })
                else {
                    complete = false;
                    continue;
                };
                if judgment.verification.as_ref().is_some_and(|report| {
                    report.status == speedrun_core::api::VerificationStatus::Unavailable
                }) {
                    unavailable = true;
                }
                let claim = current_claim(events, &milestone.milestone_id);
                let granted = claim.as_ref().map(|claim| {
                    if judgment
                        .verification
                        .as_ref()
                        .is_some_and(|report| report.verified)
                    {
                        Trust::EvidenceVerified
                    } else {
                        capped(claim.trust)
                    }
                });
                complete &= claim.as_ref().is_some_and(|claim| {
                    (template.required_trust != Trust::SelfDeclared
                        || claim.claimant != speedrun_core::event::Claimant::Agent
                        || claim.confirmed)
                        && !matches!(
                            milestone.state,
                            speedrun_core::projection::MilestoneState::Rejected
                                | speedrun_core::projection::MilestoneState::Proposed
                        )
                }) && granted.is_some_and(|trust| trust >= template.required_trust);
            }
            if unavailable {
                PublicPolicyStatus::Unavailable {
                    reason: "a required public verification is unavailable".into(),
                }
            } else if complete {
                PublicPolicyStatus::Completed
            } else {
                PublicPolicyStatus::Incomplete
            }
        }
        _ => unreachable!(),
    };
    (judgments, status)
}

fn pinned_category<'a>(
    run: &RunView,
    events: &[Event],
    categories: &'a [Category],
) -> Result<&'a Category, String> {
    let category = category::find(categories, &run.category_id, Some(run.category_version))
        .ok_or_else(|| "unknown category version".to_string())?;
    let digest = category::digest(category).map_err(|_| "cannot digest ruleset".to_string())?;
    let pinned = events.iter().find_map(|event| match &event.payload {
        EventPayload::RunStarted { ruleset_digest, .. } => ruleset_digest.as_deref(),
        _ => None,
    });
    if pinned != Some(digest.as_str()) {
        return Err("missing or mismatched ruleset digest".into());
    }
    if category
        .milestones
        .iter()
        .any(|template| template.verification_policy.is_none())
    {
        return Err("ruleset has no explicit public verification policy".into());
    }
    let proposals: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::MilestoneProposed {
                milestone_id,
                origin: MilestoneOrigin::Predefined,
                key,
                title,
                description,
                required_trust,
            } => Some((milestone_id, key, title, description, required_trust)),
            _ => None,
        })
        .collect();
    if proposals.len() != category.milestones.len() {
        return Err("predefined milestones do not match ruleset".into());
    }
    for template in &category.milestones {
        let matches: Vec<_> = proposals
            .iter()
            .filter(|(_, key, _, _, _)| key.as_ref() == Some(&template.key))
            .collect();
        let [proposal] = matches.as_slice() else {
            return Err("predefined milestones do not match ruleset".into());
        };
        let (id, _, title, description, trust) = *proposal;
        if *title != &template.title || description.as_deref() != Some(template.description.as_str()) || **trust != template.required_trust
            || events.iter().filter(|event| matches!(&event.payload, EventPayload::MilestoneProposed { milestone_id, .. } if milestone_id == *id)).count() != 1
            || events.iter().any(|event| matches!(&event.payload, EventPayload::MilestoneAccepted { milestone_id, required_trust: Some(value) } if milestone_id == *id && *value != template.required_trust))
            || run.milestones.iter().filter(|milestone| milestone.milestone_id == **id && milestone.origin == MilestoneOrigin::Predefined && milestone.required_trust == template.required_trust).count() != 1 {
            return Err("predefined milestone definition differs from ruleset".into());
        }
    }
    Ok(category)
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
    trust: Trust,
    claimant: speedrun_core::event::Claimant,
    confirmed: bool,
}

/// The last `MilestoneReached` for `milestone_id` since its last invalidation.
fn current_claim(events: &[Event], milestone_id: &str) -> Option<Claim> {
    let mut current = None;
    for event in events {
        match &event.payload {
            EventPayload::MilestoneReached {
                milestone_id: id,
                evidence,
                trust,
                claimant,
                ..
            } if id.as_str() == milestone_id => {
                current = Some(Claim {
                    event_id: event.event_id.clone(),
                    occurred_at: event.occurred_at,
                    evidence: evidence.clone(),
                    trust: *trust,
                    claimant: *claimant,
                    confirmed: false,
                });
            }
            EventPayload::MilestoneConfirmed { milestone_id: id } if id.as_str() == milestone_id => {
                if let Some(claim) = &mut current {
                    claim.confirmed = true;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use speedrun_core::event::{Claimant, SCHEMA_VERSION};

    fn event(payload: EventPayload, seq: u64) -> Event {
        let at = Utc::now();
        Event {
            event_id: seq.to_string(),
            schema_version: SCHEMA_VERSION,
            run_id: "run".into(),
            seq,
            device_id: "device".into(),
            occurred_at: at,
            recorded_at: at,
            payload,
            prev_hash: None,
            hash: String::new(),
            sig: None,
        }
    }

    #[test]
    fn public_policy_requires_pinned_matching_definitions() {
        let categories = category::builtin().unwrap();
        let category = category::find(&categories, "any-percent", Some(2)).unwrap();
        let start = EventPayload::RunStarted {
            project_id: "project".into(),
            project_root: None,
            category_id: category.id.clone(),
            category_version: category.version,
            device_pubkey: None,
            start_head: None,
            start_head_committed_at: None,
            ruleset_digest: Some(category::digest(category).unwrap()),
            disclosure_commitment: None,
        };
        let mut events = vec![event(start, 1)];
        for (index, template) in category.milestones.iter().enumerate() {
            let id = template.key.clone();
            events.push(event(
                EventPayload::MilestoneProposed {
                    milestone_id: id.clone(),
                    origin: MilestoneOrigin::Predefined,
                    key: Some(id.clone()),
                    title: template.title.clone(),
                    description: Some(template.description.clone()),
                    required_trust: template.required_trust,
                },
                index as u64 * 3 + 2,
            ));
            events.push(event(
                EventPayload::MilestoneAccepted {
                    milestone_id: id.clone(),
                    required_trust: None,
                },
                index as u64 * 3 + 3,
            ));
            events.push(event(
                EventPayload::MilestoneReached {
                    milestone_id: id,
                    trust: Trust::SelfDeclared,
                    evidence: vec![],
                    claimant: Claimant::User,
                },
                index as u64 * 3 + 4,
            ));
        }
        let check = |events: &[Event]| {
            let view = RunView::fold(events, Utc::now()).unwrap();
            judge_policy(&view, events, &verifier()).1
        };
        assert_eq!(check(&events), PublicPolicyStatus::Completed);
        let mut mismatch = events.clone();
        if let EventPayload::RunStarted { ruleset_digest, .. } = &mut mismatch[0].payload {
            *ruleset_digest = Some("wrong".into());
        }
        assert!(matches!(check(&mismatch), PublicPolicyStatus::Unavailable { .. }));
        let mut forged = events;
        if let EventPayload::MilestoneProposed { required_trust, .. } = &mut forged[1].payload {
            *required_trust = Trust::AgentClaimed;
        }
        assert!(matches!(check(&forged), PublicPolicyStatus::Unavailable { .. }));
    }
}
