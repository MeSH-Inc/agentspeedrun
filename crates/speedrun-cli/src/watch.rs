//! `speedrun watch`: a terminal live view. A reference client for the
//! live-display contract: follows the stream, ticks clocks locally.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use speedrun_core::{
    MilestoneState, RunStatus,
    live::{LiveRun, LiveSnapshot},
};

use crate::client::Client;
use crate::ops::fmt_ms;

#[derive(Default)]
struct Shared {
    snapshot: Option<LiveSnapshot>,
    connection: String,
}

pub fn watch(since_hours: u32, once: bool) -> Result<()> {
    let client = Client::new();
    client.ready()?;
    if once {
        let snap: LiveSnapshot = client.get(&format!("/v1/live?since_hours={since_hours}"))?;
        print!("{}", render(&snap, snap.as_of));
        return Ok(());
    }
    let shared = Arc::new(Mutex::new(Shared::default()));
    let writer = shared.clone();
    std::thread::spawn(move || {
        loop {
            let res = client.stream_live(since_hours, |snap| {
                let mut s = writer.lock().unwrap_or_else(|p| p.into_inner());
                // Ignore out-of-order snapshots.
                if s.snapshot.as_ref().is_none_or(|old| snap.cursor >= old.cursor) {
                    s.snapshot = Some(snap);
                }
                s.connection.clear();
            });
            if let Err(e) = res {
                writer.lock().unwrap_or_else(|p| p.into_inner()).connection = format!("reconnecting: {e}");
            }
            let _ = client.ready();
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    loop {
        let frame = {
            let s = shared.lock().unwrap_or_else(|p| p.into_inner());
            let body = s
                .snapshot
                .as_ref()
                .map_or_else(|| "connecting…\n".to_string(), |snap| render(snap, Utc::now()));
            format!("{body}{}\n", s.connection)
        };
        print!("\x1b[H\x1b[2J{frame}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn fmt_delta(ms: i64) -> String {
    let sign = if ms < 0 { '-' } else { '+' };
    let s = ms.abs() / 1000;
    format!("{sign}{}:{:02}", s / 60, s % 60)
}

fn render(snap: &LiveSnapshot, now: DateTime<Utc>) -> String {
    if snap.runs.is_empty() {
        return "No active runs. A run starts with the first prompt in a project set up with `speedrun init`.\n".into();
    }
    snap.runs
        .iter()
        .map(|r| render_run(r, snap.as_of, now))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_run(run: &LiveRun, as_of: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let t = run.timing_at(as_of, now);
    let status = match run.status {
        RunStatus::Running if run.active_agents > 0 => {
            format!("running · {} agent(s) working", run.active_agents)
        }
        RunStatus::Running => "running · waiting on you".into(),
        RunStatus::Paused => "paused".into(),
        RunStatus::Completed => "completed".into(),
        RunStatus::Abandoned => "abandoned".into(),
    };
    let mut out = format!(
        "{} · {} · {}  [{}/{}]\n  {}   timed {}  agent {}  human {}\n",
        run.project_name.as_deref().unwrap_or(&run.project_id),
        run.category.name,
        status,
        run.progress.counted,
        run.progress.total,
        fmt_ms(t.wall_ms),
        fmt_ms(t.timed_ms),
        fmt_ms(t.agent_ms),
        fmt_ms(t.human_ms),
    );
    for m in &run.milestones {
        let mark = match (m.counts, m.state) {
            (true, _) => "✔",
            (false, MilestoneState::Reached | MilestoneState::Verified) => "◐",
            (false, MilestoneState::Proposed) => "?",
            (false, MilestoneState::Rejected) => continue,
            (false, MilestoneState::Accepted) => "·",
        };
        let detail = match (m.split, m.delta_ms) {
            (Some(s), Some(d)) => format!("{}  {}", fmt_ms(s.timed_ms), fmt_delta(d)),
            (Some(s), None) => fmt_ms(s.timed_ms),
            (None, _) if m.state == MilestoneState::Proposed => "proposed".into(),
            (None, _) if m.trust.is_some() => format!("needs {:?}", m.required_trust).to_lowercase(),
            (None, _) => m
                .pb_split
                .map(|p| format!("pb {}", fmt_ms(p.timed_ms)))
                .unwrap_or_default(),
        };
        out += &format!("  {mark} {:<40} {detail}\n", m.title);
    }
    if let Some(pb) = &run.pb {
        out += &format!(
            "  pb {} ({})\n",
            fmt_ms(pb.timing.timed_ms),
            pb.project_name.as_deref().unwrap_or(&pb.run_id)
        );
    }
    out
}
