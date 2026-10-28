//! The live-display contract: what widgets, picture-in-picture views and
//! phones render. Displays never derive state themselves; they render a
//! `LiveSnapshot` and tick its clocks locally with [`LiveRun::timing_at`].
//!
//! See docs/LIVE_DISPLAY.md.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::projection::HUMAN_IDLE_CAP;
use crate::{MilestoneOrigin, MilestoneState, RunStatus, RunView, Timing, Trust};

/// Every live run worth showing, as of one instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LiveSnapshot {
    /// The instant all `timing` values were computed at.
    pub as_of: DateTime<Utc>,
    /// Monotonic position in the daemon's whole log. Larger means newer;
    /// ignore any snapshot whose cursor is lower than one already shown.
    pub cursor: u64,
    /// Unfinished runs first, then most recently active.
    pub runs: Vec<LiveRun>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LiveRun {
    pub run_id: String,
    pub project_id: String,
    /// Directory name of the project root, for display.
    pub project_name: Option<String>,
    pub category: CategoryRef,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_event_at: DateTime<Utc>,
    /// Clocks at `LiveSnapshot::as_of`.
    pub timing: Timing,
    /// Which clocks are advancing at `as_of`.
    pub running: RunningClocks,
    /// Sessions with an agent turn in progress.
    pub active_agents: u32,
    pub prompt_count: u64,
    /// Category milestones that count, out of all category milestones.
    pub progress: Progress,
    /// Category milestones first (in category order), then user and agent milestones.
    pub milestones: Vec<LiveMilestone>,
    /// Best earlier completed run of the same category and version, by `timed_ms`.
    pub pb: Option<PersonalBest>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CategoryRef {
    pub id: String,
    pub version: u32,
    pub name: String,
}

/// A clock that is running advances 1 ms per ms after `as_of`, except
/// `human`, which stops at `human_stops_at` (the idle cap).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunningClocks {
    pub wall: bool,
    pub timed: bool,
    pub agent: bool,
    pub human: bool,
    pub human_stops_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Progress {
    pub counted: u32,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct LiveMilestone {
    pub milestone_id: String,
    pub key: Option<String>,
    pub title: String,
    pub origin: MilestoneOrigin,
    pub state: MilestoneState,
    pub trust: Option<Trust>,
    pub required_trust: Trust,
    /// Reached at or above the required trust.
    pub counts: bool,
    pub split: Option<Timing>,
    /// The personal best's split for the same category milestone.
    pub pb_split: Option<Timing>,
    /// `split.timed_ms - pb_split.timed_ms`; negative is ahead of PB.
    pub delta_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PersonalBest {
    pub run_id: String,
    pub project_name: Option<String>,
    pub timing: Timing,
}

fn project_name(view: &RunView) -> Option<String> {
    let root = view.project_root.as_deref()?;
    std::path::Path::new(root)
        .file_name()?
        .to_str()
        .map(str::to_string)
}

impl LiveRun {
    /// Build from a run folded at `as_of`. `pb` is the comparison run, if any.
    pub fn from_view(
        view: &RunView,
        category_name: &str,
        as_of: DateTime<Utc>,
        pb: Option<&RunView>,
    ) -> Self {
        let live = !view.is_finished();
        let paused = view.status == RunStatus::Paused;
        let human_stops_at = view.idle_since.filter(|_| live).map(|t| t + HUMAN_IDLE_CAP);
        let running = RunningClocks {
            wall: live,
            timed: live && !paused,
            agent: live && !paused && view.open_turns > 0,
            human: human_stops_at.is_some_and(|stop| stop > as_of),
            human_stops_at: human_stops_at.filter(|stop| *stop > as_of),
        };
        let pb_split = |key: Option<&str>| {
            let key = key?;
            pb?.milestones
                .iter()
                .find(|m| m.origin == MilestoneOrigin::Predefined && m.key.as_deref() == Some(key))?
                .split
        };
        let mut milestones: Vec<LiveMilestone> = view
            .milestones
            .iter()
            .map(|m| {
                let pb_split = if m.origin == MilestoneOrigin::Predefined {
                    pb_split(m.key.as_deref())
                } else {
                    None
                };
                LiveMilestone {
                    milestone_id: m.milestone_id.clone(),
                    key: m.key.clone(),
                    title: m.title.clone(),
                    origin: m.origin,
                    state: m.state,
                    trust: m.trust,
                    required_trust: m.required_trust,
                    counts: m.counts(),
                    split: m.split,
                    pb_split,
                    delta_ms: m.split.zip(pb_split).map(|(a, b)| a.timed_ms - b.timed_ms),
                }
            })
            .collect();
        // Stable sort keeps category order within the predefined group.
        milestones.sort_by_key(|m| m.origin != MilestoneOrigin::Predefined);
        let predefined = view
            .milestones
            .iter()
            .filter(|m| m.origin == MilestoneOrigin::Predefined);
        LiveRun {
            run_id: view.run_id.clone(),
            project_id: view.project_id.clone(),
            project_name: project_name(view),
            category: CategoryRef {
                id: view.category_id.clone(),
                version: view.category_version,
                name: category_name.to_string(),
            },
            status: view.status,
            started_at: view.started_at,
            finished_at: view.finished_at,
            last_event_at: view.last_event_at,
            timing: view.timing,
            running,
            active_agents: view.open_turns,
            prompt_count: view.prompt_count,
            progress: Progress {
                counted: predefined.clone().filter(|m| m.counts()).count() as u32,
                total: predefined.count() as u32,
            },
            milestones,
            pb: pb.map(|p| PersonalBest {
                run_id: p.run_id.clone(),
                project_name: project_name(p),
                timing: p.timing,
            }),
        }
    }

    /// The clocks at `t`, extrapolated from a snapshot taken at `as_of`.
    /// Displays call this every frame instead of polling.
    pub fn timing_at(&self, as_of: DateTime<Utc>, t: DateTime<Utc>) -> Timing {
        let dt = (t - as_of).num_milliseconds().max(0);
        let r = &self.running;
        let human_end = r.human_stops_at.map_or(t, |stop| t.min(stop));
        Timing {
            wall_ms: self.timing.wall_ms + if r.wall { dt } else { 0 },
            timed_ms: self.timing.timed_ms + if r.timed { dt } else { 0 },
            agent_ms: self.timing.agent_ms + if r.agent { dt } else { 0 },
            human_ms: self.timing.human_ms
                + if r.human {
                    (human_end - as_of).num_milliseconds().max(0)
                } else {
                    0
                },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, EventPayload, Evidence, RunOutcome, SCHEMA_VERSION, hash::compute_hash};
    use chrono::Duration;

    fn t(min: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::minutes(min)
    }

    /// Deterministic run log: times in minutes from a fixed epoch.
    fn log(run_id: &str, root: &str, items: Vec<(i64, EventPayload)>) -> Vec<Event> {
        let mut all = vec![(
            0,
            EventPayload::RunStarted {
                project_id: format!("proj-{root}"),
                project_root: Some(format!("/home/dev/{root}")),
                category_id: "any-percent".into(),
                category_version: 1,
            },
        )];
        for (key, title, trust) in [
            ("scaffold-runs", "Scaffold builds and runs", Trust::SelfDeclared),
            ("happy-path-demo", "Happy-path demo", Trust::EvidenceVerified),
        ] {
            all.push((
                0,
                EventPayload::MilestoneProposed {
                    milestone_id: key.into(),
                    origin: MilestoneOrigin::Predefined,
                    key: Some(key.into()),
                    title: title.into(),
                    description: None,
                    required_trust: trust,
                },
            ));
            all.push((
                0,
                EventPayload::MilestoneAccepted {
                    milestone_id: key.into(),
                },
            ));
        }
        all.extend(items);
        let mut out: Vec<Event> = Vec::new();
        for (i, (min, payload)) in all.into_iter().enumerate() {
            let mut e = Event {
                event_id: format!("{run_id}-{i}"),
                schema_version: SCHEMA_VERSION,
                run_id: run_id.into(),
                seq: i as u64 + 1,
                device_id: "device".into(),
                occurred_at: t(min),
                recorded_at: t(min),
                payload,
                prev_hash: out.last().map(|p| p.hash.clone()),
                hash: String::new(),
            };
            e.hash = compute_hash(&e);
            out.push(e);
        }
        out
    }

    fn turn(s: &str, start: i64, end: Option<i64>) -> Vec<(i64, EventPayload)> {
        let mut v = vec![(start, EventPayload::AgentTurnStarted { session_id: s.into() })];
        v.extend(end.map(|e| (e, EventPayload::AgentTurnEnded { session_id: s.into() })));
        v
    }

    fn reach(key: &str, at: i64, trust: Trust) -> (i64, EventPayload) {
        (
            at,
            EventPayload::MilestoneReached {
                milestone_id: key.into(),
                trust,
                evidence: vec![Evidence::GitCommit {
                    sha: "4f2a9c1".into(),
                }],
            },
        )
    }

    fn prompt(at: i64) -> (i64, EventPayload) {
        (
            at,
            EventPayload::PromptSubmitted {
                session_id: "s1".into(),
                host: "claude-code".into(),
                model: None,
                content: String::new(),
                redacted: false,
            },
        )
    }

    /// A realistic snapshot: one live run with two agents working, compared
    /// against a completed personal best. Committed as an example for display work.
    fn example() -> LiveSnapshot {
        let as_of = t(50);
        let mut pb_items = vec![prompt(0)];
        pb_items.extend(turn("s1", 0, Some(30)));
        pb_items.push(reach("scaffold-runs", 12, Trust::SelfDeclared));
        pb_items.push(reach("happy-path-demo", 30, Trust::EvidenceVerified));
        pb_items.push((
            30,
            EventPayload::RunFinished {
                outcome: RunOutcome::Completed,
                reason: None,
            },
        ));
        let pb = RunView::fold(&log("01PB", "todo-app", pb_items), as_of).unwrap();

        let mut items = vec![prompt(0)];
        items.extend(turn("s1", 0, Some(9)));
        items.push(reach("scaffold-runs", 9, Trust::SelfDeclared));
        items.push(prompt(20));
        items.extend(turn("s1", 20, None));
        items.extend(turn("s2", 41, None));
        items.push(reach("happy-path-demo", 44, Trust::AgentClaimed));
        items.push((
            25,
            EventPayload::MilestoneProposed {
                milestone_id: "01AGENT".into(),
                origin: MilestoneOrigin::AgentGenerated,
                key: None,
                title: "Recipes persist across restarts".into(),
                description: None,
                required_trust: Trust::AgentClaimed,
            },
        ));
        let live = RunView::fold(&log("01LIVE", "recipe-box", items), as_of).unwrap();
        LiveSnapshot {
            as_of,
            cursor: 42,
            runs: vec![LiveRun::from_view(&live, "Any%", as_of, Some(&pb))],
        }
    }

    #[test]
    fn committed_example_is_current() {
        let committed: LiveSnapshot =
            serde_json::from_str(include_str!("../../../schema/examples/live-snapshot.json")).unwrap();
        assert!(
            committed == example(),
            "schema/examples/live-snapshot.json is stale; regenerate with \
             `cargo test -p speedrun-core -- --ignored write_example`"
        );
    }

    #[test]
    #[ignore = "writes the committed example file"]
    fn write_example() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schema/examples/live-snapshot.json"
        );
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(&example()).unwrap() + "\n").unwrap();
    }

    #[test]
    fn example_reflects_live_state() {
        let run = &example().runs[0];
        assert_eq!(run.active_agents, 2);
        assert!(run.running.wall && run.running.agent && !run.running.human);
        assert_eq!(run.progress, Progress { counted: 1, total: 2 });
        let scaffold = &run.milestones[0];
        assert_eq!(scaffold.delta_ms, Some(-3 * 60_000), "3 minutes ahead of PB");
        assert_eq!(run.milestones[2].origin, MilestoneOrigin::AgentGenerated);
    }

    #[test]
    fn extrapolation_matches_refolding() {
        // Agent stops at 10; human time then runs until the idle cap.
        let items = turn("s1", 0, Some(10));
        let events = log("r", "p", items);
        let as_of = t(12);
        let snap = LiveRun::from_view(&RunView::fold(&events, as_of).unwrap(), "Any%", as_of, None);
        assert!(snap.running.human && !snap.running.agent);
        for later in [13, 20, 25, 40] {
            let refolded = RunView::fold(&events, t(later)).unwrap().timing;
            assert_eq!(snap.timing_at(as_of, t(later)), refolded, "at minute {later}");
        }
    }

    #[test]
    fn paused_and_finished_runs_do_not_tick() {
        let paused = log("r", "p", vec![(5, EventPayload::RunPaused { reason: None })]);
        let snap = LiveRun::from_view(&RunView::fold(&paused, t(10)).unwrap(), "Any%", t(10), None);
        assert_eq!(
            snap.running,
            RunningClocks {
                wall: true,
                ..Default::default()
            }
        );
        assert_eq!(snap.timing_at(t(10), t(20)).timed_ms, snap.timing.timed_ms);
    }
}
