//! Agent-facing milestone tools. One implementation, exposed over MCP
//! (`speedrun mcp`) and as a command (`speedrun tool <name>`) for hosts
//! that register tools natively, like opencode and pi.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use speedrun_core::{Claimant, EventPayload, MilestoneOrigin, Trust};

use crate::client::Client;
use crate::ops;
use crate::project::Project;

/// Tool definitions (name, description, JSON Schema input), shared by
/// the MCP server and the native-tool adapters (`speedrun tool --list`).
pub fn definitions() -> Value {
    json!([
        {
            "name": "speedrun_status",
            "description": "Show the current timed run for this project: category, elapsed times and every milestone with its state and trust.",
            "inputSchema": {"type":"object","properties":{}}
        },
        {
            "name": "speedrun_propose_milestones",
            "description": "Propose project-specific milestones derived from the user's goal. They stay 'proposed' until the user accepts them. Make each one concrete and checkable.",
            "inputSchema": {
                "type":"object",
                "properties":{"milestones":{"type":"array","minItems":1,"maxItems":12,"items":{
                    "type":"object",
                    "properties":{"title":{"type":"string"},"description":{"type":"string"}},
                    "required":["title"]
                }}},
                "required":["milestones"]
            }
        },
        {
            "name": "speedrun_claim_milestone",
            "description": "Claim that an accepted milestone has been reached, with evidence the daemon checks: the commit that achieves it (commit first), public HTTPS URLs (the deployment; terms and privacy pages), a GitHub Actions run URL, or a Stripe payment id. Verified evidence upgrades the claim to evidence_verified; the result says what failed otherwise.",
            "inputSchema": {
                "type":"object",
                "properties":{
                    "milestone":{"type":"string","description":"Milestone id or category key, as shown by speedrun_status."},
                    "commit":{"type":"string","description":"Commit SHA or ref, e.g. HEAD."},
                    "urls":{"type":"array","items":{"type":"string"},"description":"Public HTTPS pages: the production deployment, or the terms of service and privacy policy pages."},
                    "ci_run":{"type":"string","description":"GitHub Actions run URL that passed, e.g. https://github.com/o/r/actions/runs/123."},
                    "stripe_payment":{"type":"string","description":"Stripe payment id (pi_, ch_, cs_, in_ or evt_) of a real live-mode customer payment."},
                    "note":{"type":"string"}
                },
                "required":["milestone"]
            }
        }
    ])
}

/// A proposal's identity: what it says, not when it was made or where it
/// happened to be sent. A retry names the same milestone, so the daemon can
/// treat it as a no-op while a new title still arrives.
fn proposed_milestone_id(project_id: &str, title: &str) -> String {
    speedrun_core::hash::content_id(&["milestone", project_id, title])
}

/// Why a recording tool does nothing for an unlisted project. The refusal is
/// a normal result, not an error: hosts and adapters treat it as success.
fn untrusted_message(project: &Project) -> String {
    format!(
        "This project is not trusted for this data directory; run `speedrun init --trust` in {} to record for it.",
        project.root.display()
    )
}

/// Run a tool against the project enclosing `dir`.
pub fn call(name: &str, args: &Value, dir: &Path) -> Result<String> {
    let project = Project::require(dir)?;
    let client = Client::new();
    match name {
        "speedrun_status" => Ok(match ops::active_run(&client, &project)? {
            Some(run) => ops::format_status(&run),
            None => "No active run yet; it starts with the first prompt.".into(),
        }),
        "speedrun_propose_milestones" => {
            if !project.is_trusted()? {
                return Ok(untrusted_message(&project));
            }
            let items = args
                .get("milestones")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                anyhow::bail!("milestones must be a non-empty array");
            }
            let mut queued = 0;
            let mut ids = Vec::new();
            for m in items {
                let title = m
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let milestone_id = proposed_milestone_id(&project.config.project_id, &title);
                let payload = EventPayload::MilestoneProposed {
                    milestone_id: milestone_id.clone(),
                    origin: MilestoneOrigin::AgentGenerated,
                    key: None,
                    title: title.clone(),
                    description: m.get("description").and_then(Value::as_str).map(str::to_string),
                    required_trust: Trust::AgentClaimed,
                };
                // Spooled proposals are not failures: a later flush delivers
                // them, and the daemon makes the repeated ones no-ops.
                if ops::append(&client, &project, payload)?.is_none() {
                    queued += 1;
                }
                ids.push(format!("{milestone_id}  {title}"));
            }
            let note = if queued == 0 {
                String::new()
            } else {
                format!("\n{queued} queued until the daemon is reachable.")
            };
            Ok(format!(
                "Proposed {} milestones. The user accepts them with `speedrun accept <id>`:\n{}{note}",
                ids.len(),
                ids.join("\n")
            ))
        }
        "speedrun_claim_milestone" => {
            if !project.is_trusted()? {
                return Ok(untrusted_message(&project));
            }
            let run = ops::require_run(&client, &project)?;
            let key = args.get("milestone").and_then(Value::as_str).unwrap_or_default();
            let milestone_id = ops::milestone_id(&run, key)?;
            let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
            let evidence = ops::Claim {
                commit: text("commit"),
                urls: args
                    .get("urls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                ci: text("ci_run"),
                stripe: text("stripe_payment"),
                note: text("note"),
            }
            .evidence(&project)?;
            let resp = ops::append_now(
                &Client::patient(),
                &project,
                EventPayload::MilestoneReached {
                    milestone_id,
                    trust: Trust::AgentClaimed,
                    evidence,
                    claimant: Claimant::Agent,
                },
            )?;
            let after = ops::require_run(&client, &project)?;
            let m = after.milestone(key).expect("just claimed");
            Ok(format!(
                "Claimed {}. Trust now {:?} (needs {:?}); {}.{}",
                m.title,
                m.trust.unwrap_or(Trust::AgentClaimed),
                m.required_trust,
                if m.counts() {
                    "it counts"
                } else {
                    "it does not count yet"
                },
                if resp
                    .derived
                    .iter()
                    .any(|e| matches!(e.payload, EventPayload::RunFinished { .. }))
                {
                    " The run is complete!"
                } else {
                    ""
                }
            ) + &resp
                .verification
                .map(|v| format!("\n{}", ops::format_verification(&v)))
                .unwrap_or_default())
        }
        other => anyhow::bail!("unknown tool {other}"),
    }
}
