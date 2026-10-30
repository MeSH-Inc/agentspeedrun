//! Operations shared by the CLI commands, host hooks and the MCP server.

use anyhow::{Result, bail};
use serde_json::json;
use speedrun_core::{
    EventPayload, MilestoneState, RunView,
    api::{ActiveRunResponse, AppendResponse, PromptResponse},
};

use crate::client::Client;
use crate::project::Project;

pub fn record_prompt(
    c: &Client,
    p: &Project,
    host: &str,
    session_id: &str,
    model: Option<&str>,
    content: &str,
) -> Result<Option<PromptResponse>> {
    let body = json!({
        "event_id": speedrun_core::new_id(),
        "project_id": p.config.project_id,
        "project_root": p.root,
        "category_id": p.config.category_id,
        "session_id": session_id,
        "host": host,
        "model": model,
        "content": content,
        "occurred_at": chrono::Utc::now(),
        "starts_turn": true,
    });
    Ok(c.post_durable("/v1/prompts", &body)?)
}

/// Append to the project's active run; spooled if the daemon is down.
pub fn append(c: &Client, p: &Project, payload: EventPayload) -> Result<Option<AppendResponse>> {
    let body = json!({
        "event_id": speedrun_core::new_id(),
        "occurred_at": chrono::Utc::now(),
        "payload": payload,
    });
    Ok(c.post_durable(&format!("/v1/projects/{}/events", p.config.project_id), &body)?)
}

/// Append and require the daemon to have accepted it now (interactive commands).
pub fn append_now(c: &Client, p: &Project, payload: EventPayload) -> Result<AppendResponse> {
    match append(c, p, payload)? {
        Some(r) => Ok(r),
        None => bail!("speedrund is not reachable; the change was spooled and will be sent when it is"),
    }
}

pub fn active_run(c: &Client, p: &Project) -> Result<Option<RunView>> {
    let r: ActiveRunResponse = c.get(&format!("/v1/projects/{}/active-run", p.config.project_id))?;
    Ok(r.run)
}

pub fn require_run(c: &Client, p: &Project) -> Result<RunView> {
    active_run(c, p)?
        .ok_or_else(|| anyhow::anyhow!("no active run yet: the run starts with the first prompt"))
}

/// Accept a milestone id or a category key.
pub fn milestone_id(run: &RunView, id_or_key: &str) -> Result<String> {
    match run.milestone(id_or_key) {
        Some(m) => Ok(m.milestone_id.clone()),
        None => bail!("no milestone {id_or_key:?} in this run; see `speedrun status`"),
    }
}

pub fn fmt_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn format_status(run: &RunView) -> String {
    let t = &run.timing;
    let mut out = format!(
        "{} run {} — {}\n  real {}  timed {}  agent {}  human {}  ({} prompts, {} sessions)\n\n",
        run.category_id,
        run.run_id,
        format!("{:?}", run.status).to_lowercase(),
        fmt_ms(t.wall_ms),
        fmt_ms(t.timed_ms),
        fmt_ms(t.agent_ms),
        fmt_ms(t.human_ms),
        run.prompt_count,
        run.sessions.len(),
    );
    for m in &run.milestones {
        let mark = match (m.counts(), m.state) {
            (true, _) => "✔",
            (false, MilestoneState::Reached | MilestoneState::Verified) => "◐",
            (false, MilestoneState::Proposed) => "?",
            (false, MilestoneState::Rejected) => "✗",
            (false, MilestoneState::Accepted) => "·",
        };
        let split = m
            .split
            .map(|s| format!("  @ {} (agent {})", fmt_ms(s.wall_ms), fmt_ms(s.agent_ms)))
            .unwrap_or_default();
        let trust = m
            .trust
            .map(|t| format!(" [{:?} / needs {:?}]", t, m.required_trust))
            .unwrap_or_else(|| format!(" [needs {:?}]", m.required_trust));
        out += &format!(
            "  {mark} {:<26} {}{}{}\n",
            m.key.clone().unwrap_or_else(|| m.milestone_id.clone()),
            m.title,
            trust,
            split
        );
    }
    out
}

/// Text adapters inject into the agent's context when a prompt starts a run.
pub fn run_started_context(project: &Project, run_id: &str) -> String {
    format!(
        "[agentspeedrun] This prompt started a timed `{}` run ({run_id}). The category's own milestones are already \
         tracked. Once you understand the user's goal, call the `speedrun_propose_milestones` tool once with 3-8 \
         concrete, checkable milestones specific to this project (the user accepts or rejects them). When you finish \
         one, call `speedrun_claim_milestone` with the commit that achieves it.",
        project.config.category_id
    )
}

/// One line for status bars: `any-percent 1/2 · 0:12:03`.
pub fn format_line(run: &RunView) -> String {
    let predefined = run
        .milestones
        .iter()
        .filter(|m| m.origin == speedrun_core::MilestoneOrigin::Predefined);
    let total = predefined.clone().count();
    let counted = predefined.filter(|m| m.counts()).count();
    let state = match run.status {
        speedrun_core::RunStatus::Running => String::new(),
        s => format!(" · {s:?}").to_lowercase(),
    };
    format!(
        "⏱ {} {counted}/{total} · {}{state}",
        run.category_id,
        fmt_ms(run.timing.timed_ms)
    )
}
