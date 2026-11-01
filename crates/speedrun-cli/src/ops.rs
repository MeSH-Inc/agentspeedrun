//! Operations shared by the CLI commands, host hooks and the MCP server.

use anyhow::{Result, bail};
use serde_json::json;
use speedrun_core::{
    EventPayload, Evidence, MilestoneState, RunView,
    api::{ActiveRunResponse, AppendResponse, PromptResponse, VerificationReport},
};

use crate::client::Client;
use crate::project::Project;
use crate::redacted::RedactedText;

pub fn record_prompt(
    c: &Client,
    p: &Project,
    host: &str,
    session_id: &str,
    model: Option<&str>,
    content: &str,
) -> Result<Option<PromptResponse>> {
    let content = RedactedText::new(content);
    let body = json!({
        "event_id": speedrun_core::new_id(),
        "project_id": p.config.project_id,
        "project_root": p.root,
        "category_id": p.config.category_id,
        "session_id": session_id,
        "host": host,
        "model": model,
        "content": content.as_str(),
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

/// Evidence for a milestone claim, as the CLI and the agent tools take it.
#[derive(Default)]
pub struct Claim {
    pub commit: Option<String>,
    pub urls: Vec<String>,
    pub ci: Option<String>,
    pub stripe: Option<String>,
    pub note: Option<String>,
}

impl Claim {
    pub fn evidence(self, p: &Project) -> Result<Vec<Evidence>> {
        let mut out = Vec::new();
        if let Some(rev) = self.commit {
            out.push(Evidence::GitCommit {
                sha: p.resolve_commit(&rev)?,
            });
        }
        out.extend(self.urls.into_iter().map(|url| Evidence::Url { url }));
        out.extend(self.ci.map(|url| Evidence::CiRun { url }));
        out.extend(self.stripe.map(|event_id| Evidence::StripeEvent { event_id }));
        out.extend(self.note.map(|text| Evidence::Note { text }));
        Ok(out)
    }
}

pub fn format_verification(v: &VerificationReport) -> String {
    match &v.verifier {
        Some(by) if v.verified => format!("Evidence verified ({by})."),
        _ => format!(
            "Evidence not verified; the claim keeps its trust. Fix and claim again:\n{}",
            v.problems
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
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
         one, call `speedrun_claim_milestone` with the commit that achieves it, plus the URL, GitHub Actions run \
         or Stripe payment for deployment, CI and revenue milestones; the daemon checks all of them.",
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

/// Context for hosts that can only inject at session start (Copilot).
/// Nothing once the agent has already proposed milestones for this run.
pub fn session_start_context(project: &Project, run: Option<&RunView>) -> Option<String> {
    let category = &project.config.category_id;
    match run {
        Some(r)
            if r.milestones
                .iter()
                .any(|m| m.origin == speedrun_core::MilestoneOrigin::AgentGenerated) =>
        {
            None
        }
        Some(r) => Some(format!(
            "[agentspeedrun] This project is in a timed `{}` run ({}). If you have the `speedrun_propose_milestones` \
             tool and no project-specific milestones exist yet, propose 3-8 concrete, checkable ones once you \
             understand the user's goal. When you finish one, call `speedrun_claim_milestone` with the commit \
             that achieves it (and the URL, CI run or Stripe payment where the milestone calls for one).",
            r.category_id, r.run_id
        )),
        None => Some(format!(
            "[agentspeedrun] This project is timed as a `{category}` run that starts with the user's first prompt. \
             If you have the `speedrun_propose_milestones` tool, propose 3-8 concrete, checkable milestones once \
             you understand the user's goal, and call `speedrun_claim_milestone` with the achieving commit (and URL, \
             CI run or Stripe payment where relevant) as you finish each one."
        )),
    }
}
