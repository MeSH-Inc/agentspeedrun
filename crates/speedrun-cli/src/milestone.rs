//! Contextual milestone actions shared by the terminal and host adapters.
use std::io::{self, IsTerminal, Write};

use anyhow::{Result, bail};
use serde::Serialize;
use speedrun_core::{
    Claimant, EventPayload, MilestoneState, RunView, Trust,
    api::{TestObservation, TestStatus},
    category::{self, VerificationPolicy},
};

use crate::{client::Client, ops, project::Project};

#[derive(Serialize)]
struct Item {
    id: String,
    key: Option<String>,
    title: String,
    state: MilestoneState,
    counts: bool,
    pending_confirmation: bool,
    required_trust: Trust,
    actions: Vec<Action>,
    evidence_alternatives: bool,
    evidence: Vec<EvidenceChoice>,
}

#[derive(Serialize)]
struct Action {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    primary: bool,
    destructive: bool,
}

#[derive(Serialize)]
struct EvidenceChoice {
    flag: &'static str,
    label: &'static str,
}

fn items(run: &RunView) -> Result<Vec<Item>> {
    let categories = category::builtin()?;
    let category = category::find(&categories, &run.category_id, Some(run.category_version));
    Ok(run
        .milestones
        .iter()
        .map(|m| {
            let id = m.milestone_id.clone();
            let candidates = [
                (
                    "accept",
                    EventPayload::MilestoneAccepted {
                        milestone_id: id.clone(),
                        required_trust: None,
                    },
                ),
                (
                    "reject",
                    EventPayload::MilestoneRejected {
                        milestone_id: id.clone(),
                        reason: None,
                    },
                ),
                (
                    "split",
                    EventPayload::MilestoneReached {
                        milestone_id: id.clone(),
                        trust: Trust::SelfDeclared,
                        evidence: vec![],
                        claimant: Claimant::User,
                    },
                ),
                (
                    "confirm",
                    EventPayload::MilestoneConfirmed {
                        milestone_id: id.clone(),
                    },
                ),
                (
                    "invalidate",
                    EventPayload::MilestoneInvalidated {
                        milestone_id: id,
                        reason: "reason".into(),
                    },
                ),
            ];
            let mut actions: Vec<Action> = candidates
                .into_iter()
                .filter_map(|(id, event)| {
                    if run.check(&event).is_err() {
                        return None;
                    }
                    let (label, description, primary, destructive) = match id {
                        "accept" => (
                            "Accept proposal",
                            "Add this proposed milestone to the run",
                            true,
                            false,
                        ),
                        "reject" => ("Reject proposal", "Remove this proposed milestone", false, true),
                        "split" if m.state == MilestoneState::Accepted => (
                            "Mark complete",
                            "Record your claim that this milestone is complete",
                            true,
                            false,
                        ),
                        "split" => (
                            "Attach or replace evidence",
                            "Record another claim and evidence",
                            !m.counts() && !m.pending_confirmation,
                            false,
                        ),
                        "confirm" => (
                            "Confirm agent claim",
                            "Confirm that the agent's claim is complete",
                            true,
                            false,
                        ),
                        "invalidate" => (
                            "Invalidate claim",
                            "Clear the recorded claim and evidence",
                            false,
                            true,
                        ),
                        _ => unreachable!(),
                    };
                    Some(Action {
                        id,
                        label,
                        description,
                        primary,
                        destructive,
                    })
                })
                .collect();
            actions.sort_by_key(|action| !action.primary);
            let policy = category
                .and_then(|c| {
                    c.milestones
                        .iter()
                        .find(|t| m.key.as_deref() == Some(t.key.as_str()))
                })
                .and_then(|t| t.verification_policy);
            let legacy_tests = policy.is_none() && m.key.as_deref() == Some("tests-green");
            let policy = policy.or(match m.key.as_deref() {
                Some("deployed") => Some(VerificationPolicy::Deployment),
                Some("legal-pages") => Some(VerificationPolicy::LegalPages),
                Some("tests-green") => Some(VerificationPolicy::LocalTestsOrCi),
                Some("first-payment") => Some(VerificationPolicy::FirstPayment),
                _ => None,
            });
            let evidence = match policy {
                Some(VerificationPolicy::LegalPages) => vec![
                    EvidenceChoice {
                        flag: "--url",
                        label: "Terms page URL",
                    },
                    EvidenceChoice {
                        flag: "--url",
                        label: "Privacy page URL",
                    },
                ],
                Some(VerificationPolicy::Deployment) => {
                    vec![EvidenceChoice {
                        flag: "--url",
                        label: "Public URL",
                    }]
                }
                Some(VerificationPolicy::LocalTestsOrCi) if legacy_tests => vec![EvidenceChoice {
                    flag: "--ci",
                    label: "CI run URL",
                }],
                Some(VerificationPolicy::LocalTestsOrCi) => vec![
                    EvidenceChoice {
                        flag: "--local-test",
                        label: "Local test observation",
                    },
                    EvidenceChoice {
                        flag: "--ci",
                        label: "CI run URL",
                    },
                ],
                Some(VerificationPolicy::FirstPayment) => vec![EvidenceChoice {
                    flag: "--stripe",
                    label: "Stripe payment or event ID",
                }],
                _ => vec![],
            };
            Item {
                id: m.milestone_id.clone(),
                key: m.key.clone(),
                title: m.title.clone(),
                state: m.state,
                counts: m.counts(),
                pending_confirmation: m.pending_confirmation,
                required_trust: m.required_trust,
                actions,
                evidence_alternatives: policy == Some(VerificationPolicy::LocalTestsOrCi) && !legacy_tests,
                evidence,
            }
        })
        .collect())
}

fn read(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        bail!("input ended");
    }
    Ok(line.trim().to_owned())
}

fn parse_selection(answer: &str, len: usize) -> Option<Option<usize>> {
    if answer.is_empty() || answer.eq_ignore_ascii_case("q") {
        return Some(None);
    }
    let n: usize = answer.parse().ok()?;
    (n > 0 && n <= len).then(|| Some(n - 1))
}

fn pick(prompt: &str, labels: &[String]) -> Result<Option<usize>> {
    for (i, label) in labels.iter().enumerate() {
        println!("  {}. {label}", i + 1);
    }
    loop {
        if let Some(selection) = parse_selection(&read(prompt)?, labels.len()) {
            return Ok(selection);
        }
        println!("Enter a number from 1 to {}, or q to cancel.", labels.len());
    }
}

fn observation_label(observation: &TestObservation, head: Option<&str>) -> String {
    let status = match &observation.status {
        TestStatus::Running => "running".to_owned(),
        TestStatus::Interrupted => "interrupted".to_owned(),
        TestStatus::Success => "passed".to_owned(),
        TestStatus::Failed {
            exit_code: Some(code),
        } => format!("failed (exit {code})"),
        TestStatus::Failed { exit_code: None } => "failed".to_owned(),
        TestStatus::TimedOut => "timed out".to_owned(),
        TestStatus::ContextChanged => "context changed".to_owned(),
    };
    let relevance = match head {
        Some(head) if head == observation.head => "current HEAD",
        Some(_) => "different HEAD",
        None => "current HEAD unavailable",
    };
    format!(
        "{status} — {} — {relevance}",
        observation.observed_at.to_rfc3339()
    )
}

fn pick_observation(c: &Client, p: &Project, run: &RunView) -> Result<Option<String>> {
    let observations: Vec<TestObservation> = c
        .get(&format!("/v1/runs/{}/local-tests", run.run_id))
        .map_err(|error| anyhow::anyhow!("could not load local test history: {error}"))?;
    if observations.is_empty() {
        println!(
            "No local test observations for this run. Run `speedrun local-test -- <command> [args...]` first; no claim was recorded."
        );
        return Ok(None);
    }
    let head = p.resolve_commit("HEAD").ok();
    let labels = observations
        .iter()
        .map(|o| observation_label(o, head.as_deref()))
        .collect::<Vec<_>>();
    Ok(pick("Local test (number, or q to cancel): ", &labels)?.map(|index| observations[index].id.clone()))
}

pub fn run(c: &Client, p: &Project, id: Option<&str>, json: bool) -> Result<()> {
    let run = ops::require_run(c, p)?;
    let mut choices = items(&run)?;
    if let Some(id) = id {
        let selected = ops::milestone_id(&run, id)?;
        choices.retain(|m| m.id == selected);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "version": 1, "milestones": choices }))?
        );
        return Ok(());
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive milestone actions require a terminal; use --json to list actions");
    }
    if choices.is_empty() {
        bail!("no milestones in this run");
    }
    let index = if choices.len() == 1 {
        0
    } else {
        let Some(index) = pick(
            "Milestone (number, or q to cancel): ",
            &choices
                .iter()
                .map(|m| {
                    format!(
                        "{} ({:?}{})",
                        m.title,
                        m.state,
                        if m.pending_confirmation {
                            ", awaiting confirmation"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>(),
        )?
        else {
            return Ok(());
        };
        index
    };
    let m = &choices[index];
    println!(
        "{}: {:?}, {}{}",
        m.title,
        m.state,
        if m.counts { "counted" } else { "not counted" },
        if m.pending_confirmation {
            ", awaiting confirmation"
        } else {
            ""
        }
    );
    if m.actions.is_empty() {
        println!("No actions available.");
        return Ok(());
    }
    let Some(index) = pick(
        "Action (number, or q to cancel): ",
        &m.actions
            .iter()
            .map(|a| {
                format!(
                    "{} — {}{}",
                    a.label,
                    a.description,
                    if a.primary { "" } else { " (advanced)" }
                )
            })
            .collect::<Vec<_>>(),
    )?
    else {
        return Ok(());
    };
    let action = m.actions[index].id;
    let payload = match action {
        "accept" => EventPayload::MilestoneAccepted {
            milestone_id: m.id.clone(),
            required_trust: None,
        },
        "confirm" => EventPayload::MilestoneConfirmed {
            milestone_id: m.id.clone(),
        },
        "reject" | "invalidate" => {
            if read(&format!("{action} this milestone? [y/N] "))?.to_lowercase() != "y" {
                return Ok(());
            }
            let reason = read("Reason: ")?;
            if action == "reject" {
                EventPayload::MilestoneRejected {
                    milestone_id: m.id.clone(),
                    reason: (!reason.is_empty()).then_some(reason),
                }
            } else {
                if reason.is_empty() {
                    bail!("a reason is required");
                }
                EventPayload::MilestoneInvalidated {
                    milestone_id: m.id.clone(),
                    reason,
                }
            }
        }
        "split" => {
            let mut claim = ops::Claim::default();
            let mut local_test = None;
            if !m.evidence.is_empty() {
                println!(
                    "Evidence can be checked by the daemon; verification is not guaranteed. Leave blank to skip."
                );
                let selected: Vec<&EvidenceChoice> = if m.evidence_alternatives {
                    let mut labels = m.evidence.iter().map(|e| e.label.to_string()).collect::<Vec<_>>();
                    labels.push("Record without evidence (may not count until required trust is met)".into());
                    let Some(index) = pick("Evidence type (number, or q to cancel): ", &labels)? else {
                        return Ok(());
                    };
                    m.evidence.get(index).into_iter().collect()
                } else {
                    m.evidence.iter().collect()
                };
                for choice in selected {
                    if choice.flag == "--local-test" {
                        let Some(id) = pick_observation(c, p, &run)? else {
                            return Ok(());
                        };
                        local_test = Some(id);
                        continue;
                    }
                    let value = read(&format!("{}: ", choice.label))?;
                    if value.is_empty() {
                        continue;
                    }
                    match choice.flag {
                        "--url" => claim.urls.push(value),
                        "--ci" => claim.ci = Some(value),
                        "--stripe" => claim.stripe = Some(value),
                        _ => {}
                    }
                }
            }
            let mut evidence = claim.evidence(p)?;
            if let Some(observation_id) = local_test {
                evidence.push(speedrun_core::Evidence::LocalTest { observation_id });
            }
            EventPayload::MilestoneReached {
                milestone_id: m.id.clone(),
                trust: Trust::SelfDeclared,
                evidence,
                claimant: Claimant::User,
            }
        }
        _ => unreachable!(),
    };
    let response = ops::append_now(&Client::patient(), p, payload)?;
    if let Some(report) = &response.verification {
        println!("{}", ops::format_verification(report));
    }
    if let Some(job) = &response.verification_job {
        println!(
            "Evidence check {:?}; inspect with `speedrun verification {}`.",
            job.state, job.cause_event_id
        );
    }
    print!("{}", ops::format_status(&ops::require_run(c, p)?));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(category: &str, version: u32, key: &str, state: &str, pending: bool) -> RunView {
        serde_json::from_value(json!({
            "run_id":"run", "project_id":"project", "project_root":null,
            "category_id":category, "category_version":version, "status":"running",
            "started_at":"2026-01-01T00:00:00Z", "finished_at":null, "closed":false,
            "prompt_count":0, "sessions":[], "hosts":[], "timing":{"wall_ms":0,"timed_ms":0,"agent_ms":0,"human_ms":0},
            "open_turns":0, "paused_since":null, "idle_since":null, "last_event_at":"2026-01-01T00:00:00Z", "last_seq":0, "head_hash":null,
            "milestones":[{"milestone_id":"m", "key":key, "origin":"predefined", "title":"Test", "description":null,
                "required_trust":"self_declared", "state":state, "trust":null, "evidence":[], "reached_at":null,
                "counted_at":null, "split":null, "pending_confirmation":pending}]
        })).unwrap()
    }

    #[test]
    fn observation_labels_report_failure_and_head_without_arguments() {
        let observation: TestObservation = serde_json::from_value(json!({
            "id": "observation", "run_id": "run", "project_id": "project",
            "head": "abc", "argv": ["secret-argument"],
            "status": {"kind": "failed", "exit_code": 2},
            "observed_at": "2026-01-01T00:00:00Z", "observer": "test", "scope": "local"
        }))
        .unwrap();
        let current = observation_label(&observation, Some("abc"));
        assert!(current.contains("failed (exit 2)") && current.contains("current HEAD"));
        assert!(current.contains("2026-01-01"));
        assert!(!current.contains("secret-argument"));
        assert!(observation_label(&observation, Some("def")).contains("different HEAD"));
        assert!(observation_label(&observation, None).contains("current HEAD unavailable"));
    }

    #[test]
    fn invalid_selections_can_be_retried_or_cancelled() {
        assert_eq!(parse_selection("0", 2), None);
        assert_eq!(parse_selection("no", 2), None);
        assert_eq!(parse_selection("2", 2), Some(Some(1)));
        assert_eq!(parse_selection("q", 2), Some(None));
    }

    #[test]
    fn actions_follow_lifecycle_and_finished_runs_are_frozen() {
        let mut run = fixture("any-percent", 2, "scaffold-runs", "proposed", false);
        assert_eq!(
            items(&run).unwrap()[0]
                .actions
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            ["accept", "reject"]
        );
        run.milestones[0].state = MilestoneState::Reached;
        run.milestones[0].pending_confirmation = true;
        assert_eq!(
            items(&run).unwrap()[0]
                .actions
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            ["confirm", "split", "invalidate"]
        );
        run.status = speedrun_core::RunStatus::Completed;
        assert!(items(&run).unwrap()[0].actions.is_empty());
    }

    #[test]
    fn evidence_tracks_the_pinned_policy_and_legacy_rules() {
        let semantic = fixture("any-percent", 2, "scaffold-runs", "accepted", false);
        assert!(items(&semantic).unwrap()[0].evidence.is_empty());
        let tests = fixture("hundred-percent", 2, "tests-green", "accepted", false);
        let flags: Vec<_> = items(&tests).unwrap()[0]
            .evidence
            .iter()
            .map(|e| e.flag)
            .collect();
        assert_eq!(flags, ["--local-test", "--ci"]);
        let legacy = fixture("hundred-percent", 1, "tests-green", "accepted", false);
        let flags: Vec<_> = items(&legacy).unwrap()[0]
            .evidence
            .iter()
            .map(|e| e.flag)
            .collect();
        assert_eq!(flags, ["--ci"]);
    }
}
