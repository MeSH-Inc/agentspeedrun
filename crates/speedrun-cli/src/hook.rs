//! Host hook adapters. Hooks must never break the host: every failure is
//! swallowed (logged with SPEEDRUN_DEBUG=1) and the process exits 0.

use std::io::Read;

use anyhow::Result;
use serde_json::{Value, json};
use speedrun_core::EventPayload;

use crate::client::Client;
use crate::ops;
use crate::project::Project;

pub fn claude_code(event: &str) {
    if let Err(e) = claude_code_inner(event)
        && std::env::var_os("SPEEDRUN_DEBUG").is_some()
    {
        eprintln!("speedrun hook: {e:#}");
    }
}

/// Claude Code passes hook input as JSON on stdin.
/// https://docs.claude.com/en/docs/claude-code/hooks
fn claude_code_inner(event: &str) -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: Value = serde_json::from_str(&input)?;
    let str_of = |k: &str| v.get(k).and_then(Value::as_str);
    let cwd = str_of("cwd")
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let Some(project) = Project::find(&cwd)? else {
        return Ok(()); // Not a tracked project.
    };
    let session = str_of("session_id").unwrap_or("unknown");
    let client = Client::new();
    match event {
        "user-prompt-submit" => {
            let prompt = str_of("prompt").unwrap_or_default();
            let resp =
                ops::record_prompt(&client, &project, "claude-code", session, str_of("model"), prompt)?;
            if let Some(r) = resp.filter(|r| r.run_started) {
                print_context(&run_started_context(&project, &r.run_id));
            }
        }
        "stop" | "session-end" => {
            ops::append(
                &client,
                &project,
                EventPayload::AgentTurnEnded {
                    session_id: session.to_string(),
                },
            )?;
        }
        other => anyhow::bail!("unknown claude-code hook event {other}"),
    }
    Ok(())
}

fn print_context(text: &str) {
    let out = json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text,
        }
    });
    println!("{out}");
}

fn run_started_context(project: &Project, run_id: &str) -> String {
    format!(
        "[agentspeedrun] This prompt started a timed `{}` run ({run_id}). The category's own milestones are already \
         tracked. Once you understand the user's goal, call the `speedrun_propose_milestones` tool once with 3-8 \
         concrete, checkable milestones specific to this project (the user accepts or rejects them). When you finish \
         one, call `speedrun_claim_milestone` with the commit that achieves it.",
        project.config.category_id
    )
}
