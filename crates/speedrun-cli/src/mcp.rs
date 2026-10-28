//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0) that
//! lets agents read the run and propose or claim milestones.

use std::io::{BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};
use speedrun_core::{EventPayload, Evidence, MilestoneOrigin, Trust};

use crate::client::Client;
use crate::ops;
use crate::project::Project;

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            write(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}),
            )?;
            continue;
        };
        // Notifications have no id and get no response.
        let Some(id) = msg.get("id").cloned() else {
            continue;
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL_VERSION);
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "agentspeedrun", "version": env!("CARGO_PKG_VERSION")},
                }})
            }
            "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
            "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools": tools()}}),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match call(name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (format!("{e:#}"), true),
                };
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "content":[{"type":"text","text":text}],
                    "isError": is_error,
                }})
            }
            _ => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":format!("method not found: {method}")}})
            }
        };
        write(&mut stdout, response)?;
    }
    Ok(())
}

fn write(out: &mut impl Write, v: Value) -> Result<()> {
    writeln!(out, "{v}")?;
    out.flush()?;
    Ok(())
}

fn tools() -> Value {
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

fn call(name: &str, args: &Value) -> Result<String> {
    let project = Project::require(&std::env::current_dir()?)?;
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
