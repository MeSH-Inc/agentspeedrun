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
    report(claude_code_inner(event));
}

pub fn copilot(event: &str) {
    report(copilot_inner(event));
}

fn report(result: Result<()>) {
    if let Err(e) = result
        && std::env::var_os("SPEEDRUN_DEBUG").is_some()
    {
        eprintln!("speedrun hook: {e:#}");
    }
}

/// Read hook input; fields may be snake_case (Claude-style, Copilot's
/// PascalCase aliases) or camelCase (Copilot's native event names).
fn read_input() -> Result<impl Fn(&str, &str) -> Option<String>> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let v: Value = serde_json::from_str(&input)?;
    Ok(move |snake: &str, camel: &str| {
        v.get(snake)
            .or_else(|| v.get(camel))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn find_project(cwd: Option<String>) -> Result<Option<Project>> {
    let cwd = cwd
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    Project::find(&cwd)
}

/// GitHub Copilot agent hooks, used by VS Code's agent and the Copilot CLI.
/// https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-hooks-reference
///
/// Copilot ignores output from prompt hooks, so the run-start nudge goes
/// out at session start instead.
fn copilot_inner(event: &str) -> Result<()> {
    let field = read_input()?;
    let Some(project) = find_project(field("cwd", "cwd"))? else {
        return Ok(());
    };
    let session = field("session_id", "sessionId").unwrap_or_else(|| "unknown".into());
    let client = Client::new();
    match event {
        "session-start" => {
            let run = ops::active_run(&client, &project).ok().flatten();
            if let Some(text) = ops::session_start_context(&project, run.as_ref()) {
                let out = json!({
                    "additionalContext": text,
                    "hookSpecificOutput": { "hookEventName": "SessionStart", "additionalContext": text },
                });
                println!("{out}");
            }
        }
        "user-prompt-submit" => {
            let prompt = field("prompt", "prompt").unwrap_or_default();
            ops::record_prompt(&client, &project, "copilot", &session, None, &prompt)?;
        }
        "stop" | "session-end" => {
            ops::append(
                &client,
                &project,
                EventPayload::AgentTurnEnded { session_id: session },
            )?;
        }
        other => anyhow::bail!("unknown copilot hook event {other}"),
    }
    Ok(())
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
                print_context(&ops::run_started_context(&project, &r.run_id));
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
