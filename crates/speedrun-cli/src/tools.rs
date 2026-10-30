//! Agent-facing milestone tools. One implementation, exposed over MCP
//! (`speedrun mcp`) and as a command (`speedrun tool <name>`) for hosts
//! that register tools natively, like opencode and pi.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};
use speedrun_core::{EventPayload, Evidence, MilestoneOrigin, Trust};

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
            "description": "Claim that an accepted milestone has been reached. Attach the commit that achieves it (commit it first); the daemon verifies commits and upgrades the claim to evidence_verified.",
            "inputSchema": {
                "type":"object",
                "properties":{
                    "milestone":{"type":"string","description":"Milestone id or category key, as shown by speedrun_status."},
                    "commit":{"type":"string","description":"Commit SHA or ref, e.g. HEAD."},
                    "urls":{"type":"array","items":{"type":"string"}},
                    "note":{"type":"string"}
                },
                "required":["milestone"]
            }
        }
    ])
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
            let items = args
                .get("milestones")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                anyhow::bail!("milestones must be a non-empty array");
            }
            let mut ids = Vec::new();
            for m in items {
                let title = m
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let milestone_id = speedrun_core::new_id();
                ops::append_now(
                    &client,
                    &project,
                    EventPayload::MilestoneProposed {
                        milestone_id: milestone_id.clone(),
                        origin: MilestoneOrigin::AgentGenerated,
                        key: None,
                        title: title.clone(),
                        description: m.get("description").and_then(Value::as_str).map(str::to_string),
                        required_trust: Trust::AgentClaimed,
                    },
                )?;
                ids.push(format!("{milestone_id}  {title}"));
            }
            Ok(format!(
                "Proposed {} milestones. The user accepts them with `speedrun accept <id>`:\n{}",
                ids.len(),
                ids.join("\n")
            ))
        }
        "speedrun_claim_milestone" => {
            let run = ops::require_run(&client, &project)?;
            let key = args.get("milestone").and_then(Value::as_str).unwrap_or_default();
            let milestone_id = ops::milestone_id(&run, key)?;
            let mut evidence = Vec::new();
            if let Some(rev) = args.get("commit").and_then(Value::as_str) {
                evidence.push(Evidence::GitCommit {
                    sha: project.resolve_commit(rev)?,
                });
            }
            for url in args
                .get("urls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                evidence.push(Evidence::Url { url: url.to_string() });
            }
            if let Some(text) = args.get("note").and_then(Value::as_str) {
                evidence.push(Evidence::Note {
                    text: text.to_string(),
                });
            }
            let resp = ops::append_now(
                &client,
                &project,
                EventPayload::MilestoneReached {
                    milestone_id,
                    trust: Trust::AgentClaimed,
                    evidence,
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
            ))
        }
        other => anyhow::bail!("unknown tool {other}"),
    }
}
