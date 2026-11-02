//! Fold a run's event log into its current state and timing.
//!
//! Projections are pure functions of the log, so timing rules can change and
//! old runs can be recomputed without migrating stored data.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Claimant, Event, EventPayload, Evidence, MilestoneOrigin, RunOutcome, Trust};

/// Gaps between agent activity longer than this are treated as the human
/// being away, and only this much of them counts as human time.
pub const HUMAN_IDLE_CAP: Duration = Duration::minutes(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Paused,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MilestoneState {
    Proposed,
    Accepted,
    Rejected,
    Reached,
    Verified,
}

/// Elapsed time measured several ways, all in milliseconds since run start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Timing {
    /// Real time: wall clock since the first prompt.
    pub wall_ms: i64,
    /// Real time minus explicit pauses.
    pub timed_ms: i64,
    /// Time at least one agent turn was running (union across sessions), minus pauses.
    pub agent_ms: i64,
    /// Time between agent turns, each gap capped at the idle threshold, minus pauses.
    pub human_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MilestoneView {
    pub milestone_id: String,
    pub key: Option<String>,
    pub origin: MilestoneOrigin,
    pub title: String,
    pub description: Option<String>,
    pub required_trust: Trust,
    pub state: MilestoneState,
    /// Highest trust any claim or verification has reached.
    pub trust: Option<Trust>,
    pub evidence: Vec<Evidence>,
    pub reached_at: Option<DateTime<Utc>>,
    /// When the milestone first met its required trust. This is the split.
    pub counted_at: Option<DateTime<Utc>>,
    pub split: Option<Timing>,
    /// An agent's claim holds this milestone and awaits the user's
    /// `milestone_confirmed`; until then the milestone does not count.
    #[serde(default)]
    pub pending_confirmation: bool,
}

impl MilestoneView {
    /// Whether the strongest claim meets this milestone's trust requirement.
    fn meets_trust_requirement(&self) -> bool {
        match self.trust {
            Some(t) if t >= self.required_trust => true,
            // Outside category rules a user's own claim stands in for an agent
            // claim: the user is the authority on milestones they created.
            Some(Trust::SelfDeclared) => {
                self.origin != MilestoneOrigin::Predefined && self.required_trust == Trust::AgentClaimed
            }
            _ => false,
        }
    }

    /// Whether the milestone counts toward the run. A claim awaiting the
    /// user's confirmation does not count, whatever its trust (D6).
    pub fn counts(&self) -> bool {
        !self.pending_confirmation
            && matches!(self.state, MilestoneState::Reached | MilestoneState::Verified)
            && self.meets_trust_requirement()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RunView {
    pub run_id: String,
    pub project_id: String,
    pub project_root: Option<String>,
    pub category_id: String,
    pub category_version: u32,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// No further prompts go to this run: it was abandoned, or reset after completing.
    pub closed: bool,
    pub prompt_count: u64,
    pub sessions: BTreeSet<String>,
    pub hosts: BTreeSet<String>,
    pub milestones: Vec<MilestoneView>,
    /// Clocks as of the `now` the view was folded at (or completion).
    pub timing: Timing,
    /// Sessions with an agent turn in progress.
    pub open_turns: u32,
    /// Start of the current pause, if paused.
    pub paused_since: Option<DateTime<Utc>>,
    /// Start of the current gap between agent turns, if the run is live, unpaused and no turn is open.
    pub idle_since: Option<DateTime<Utc>>,
    /// When the daemon recorded the latest event.
    pub last_event_at: DateTime<Utc>,
    pub last_seq: u64,
    pub head_hash: Option<String>,
}

type Interval = (DateTime<Utc>, DateTime<Utc>);

/// One observed agent turn for a session.
#[derive(Debug, Clone, Copy)]
struct Turn {
    start: DateTime<Utc>,
    /// When the turn ended, or provisionally when a newer turn started first.
    end: Option<DateTime<Utc>>,
    /// `end` was inferred from a newer start, so a late real end may extend it.
    provisional: bool,
}

impl Turn {
    /// Whether an `AgentTurnEnded` at `t` can pair with this turn.
    fn accepts_end(&self, t: DateTime<Utc>) -> bool {
        self.start <= t && (self.end.is_none() || (self.provisional && self.end.is_some_and(|e| e <= t)))
    }
}

/// Sort and merge overlapping intervals.
fn merge(mut v: Vec<Interval>) -> Vec<Interval> {
    v.retain(|(a, b)| b > a);
    v.sort();
    let mut out: Vec<Interval> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Intersection of two merged interval lists.
fn intersect(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    for &(a0, a1) in a {
        for &(b0, b1) in b {
            let (s, e) = (a0.max(b0), a1.min(b1));
            if e > s {
                out.push((s, e));
            }
        }
    }
    merge(out)
}

/// Milliseconds of `merged` that fall inside `[from, to]`.
fn covered_ms(merged: &[Interval], from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
    merged
        .iter()
        .map(|&(a, b)| (b.min(to) - a.max(from)).num_milliseconds().max(0))
        .sum()
}

/// Sum of the gaps in `busy` within `[from, to]`, each capped at `cap`.
fn capped_gaps_ms(busy: &[Interval], from: DateTime<Utc>, to: DateTime<Utc>, cap: Duration) -> i64 {
    let cap = cap.num_milliseconds();
    let mut total = 0;
    let mut cursor = from;
    for &(a, b) in busy {
        if a >= to {
            break;
        }
        if a > cursor {
            total += (a - cursor).num_milliseconds().min(cap);
        }
        cursor = cursor.max(b);
    }
    if to > cursor {
        total += (to - cursor).num_milliseconds().min(cap);
    }
    total
}

struct Clock {
    start: DateTime<Utc>,
    agent: Vec<Interval>,
    pauses: Vec<Interval>,
    busy: Vec<Interval>,
}

impl Clock {
    fn new(start: DateTime<Utc>, agent: Vec<Interval>, pauses: Vec<Interval>) -> Self {
        let pauses = merge(pauses);
        let agent = merge(agent);
        let busy = merge(agent.iter().chain(pauses.iter()).copied().collect());
        let agent_paused = intersect(&agent, &pauses);
        // Agent time excludes paused stretches.
        let agent = subtract(&agent, &agent_paused);
        Self {
            start,
            agent,
            pauses,
            busy,
        }
    }

    fn at(&self, t: DateTime<Utc>) -> Timing {
        let t = t.max(self.start);
        let wall_ms = (t - self.start).num_milliseconds();
        let paused = covered_ms(&self.pauses, self.start, t);
        Timing {
            wall_ms,
            timed_ms: wall_ms - paused,
            agent_ms: covered_ms(&self.agent, self.start, t),
            human_ms: capped_gaps_ms(&self.busy, self.start, t, HUMAN_IDLE_CAP),
        }
    }
}

/// `a` minus `b`, both merged.
fn subtract(a: &[Interval], b: &[Interval]) -> Vec<Interval> {
    let mut out = Vec::new();
    for &(mut s, e) in a {
        for &(b0, b1) in b {
            if b1 <= s || b0 >= e {
                continue;
            }
            if b0 > s {
                out.push((s, b0));
            }
            s = s.max(b1);
        }
        if e > s {
            out.push((s, e));
        }
    }
    merge(out)
}

/// The order `fold` reads a run's events in. A single device's chain is read
/// exactly as given, so one chain is never reordered. Several chains are
/// merged by `occurred_at`, each still in its own order, so clock skew can
/// interleave devices but never reorder one device's chain.
fn fold_order(events: &[Event]) -> Vec<&Event> {
    let chains = crate::event::device_chains(events);
    if chains.len() <= 1 {
        return events.iter().collect();
    }
    let mut heads: Vec<VecDeque<&Event>> = chains.into_iter().map(VecDeque::from).collect();
    let mut out = Vec::with_capacity(events.len());
    loop {
        let next = heads
            .iter_mut()
            .filter(|chain| !chain.is_empty())
            .min_by_key(|chain| chain.front().map(|event| event.occurred_at));
        let Some(chain) = next else {
            break;
        };
        let Some(event) = chain.pop_front() else {
            break;
        };
        out.push(event);
    }
    out
}

impl RunView {
    /// Fold a run's events, which may hold one chain per device. `now` closes
    /// any open turn or pause. Returns `None` when the run has no
    /// `RunStarted`, wherever in the input it sits.
    pub fn fold(events: &[Event], now: DateTime<Utc>) -> Option<RunView> {
        let first = events
            .iter()
            .find(|e| matches!(e.payload, EventPayload::RunStarted { .. }))?;
        let EventPayload::RunStarted {
            project_id,
            project_root,
            category_id,
            category_version,
            ..
        } = &first.payload
        else {
            return None;
        };
        let start = first.occurred_at;
        let mut view = RunView {
            run_id: first.run_id.clone(),
            project_id: project_id.clone(),
            project_root: project_root.clone(),
            category_id: category_id.clone(),
            category_version: *category_version,
            status: RunStatus::Running,
            started_at: start,
            finished_at: None,
            closed: false,
            prompt_count: 0,
            sessions: BTreeSet::new(),
            hosts: BTreeSet::new(),
            milestones: Vec::new(),
            timing: Timing::default(),
            open_turns: 0,
            paused_since: None,
            idle_since: None,
            last_event_at: first.recorded_at,
            last_seq: 0,
            head_hash: None,
        };
        let mut turns: BTreeMap<String, Vec<Turn>> = BTreeMap::new();
        // Latest activity seen per session, used to end a superseded turn.
        let mut last_seen: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
        let mut agent: Vec<Interval> = Vec::new();
        let mut pauses: Vec<Interval> = Vec::new();
        let mut pause_open: Option<DateTime<Utc>> = None;
        // (milestone index, time it started counting); splits computed after timing is known.
        let mut index: BTreeMap<String, usize> = BTreeMap::new();

        for e in fold_order(events) {
            view.last_seq = e.seq;
            view.head_hash = Some(e.hash.clone());
            view.last_event_at = view.last_event_at.max(e.recorded_at);
            let t = e.occurred_at.max(start);
            match &e.payload {
                EventPayload::RunStarted { .. }
                | EventPayload::DeviceJoined { .. }
                | EventPayload::DeviceRotated { .. } => {}
                EventPayload::PromptSubmitted { session_id, host, .. } => {
                    view.prompt_count += 1;
                    view.sessions.insert(session_id.clone());
                    view.hosts.insert(host.clone());
                    // A prompt means the user is back: end any open pause here.
                    if let Some(p) = pause_open.take() {
                        pauses.push((p, t));
                        view.status = RunStatus::Running;
                    }
                    let seen = last_seen.get(session_id).copied();
                    last_seen.insert(session_id.clone(), seen.map_or(t, |s| s.max(t)));
                }
                EventPayload::AgentTurnStarted { session_id } => {
                    view.sessions.insert(session_id.clone());
                    // Agent work means the user is back: end any open pause here.
                    if let Some(p) = pause_open.take() {
                        pauses.push((p, t));
                        view.status = RunStatus::Running;
                    }
                    // A newer start supersedes an open turn for this session. Close
                    // it at the last thing we saw, not at this (possibly much later)
                    // start, so a crashed turn cannot absorb the idle gap.
                    let seen = last_seen.get(session_id).copied();
                    if let Some(list) = turns.get_mut(session_id) {
                        for turn in list.iter_mut() {
                            if turn.end.is_none() && turn.start <= t {
                                turn.end = Some(seen.map_or(turn.start, |s| s.max(turn.start)));
                                turn.provisional = true;
                            }
                        }
                    }
                    let turn = Turn {
                        start: t,
                        end: None,
                        provisional: false,
                    };
                    turns.entry(session_id.clone()).or_default().push(turn);
                    last_seen.insert(session_id.clone(), seen.map_or(t, |s| s.max(t)));
                }
                EventPayload::AgentTurnEnded { session_id } => {
                    // Pair by occurred_at: an end can belong to a start delivered
                    // after it, and an end before every known start is stale.
                    if let Some(list) = turns.get_mut(session_id) {
                        let candidate = list
                            .iter_mut()
                            .filter(|turn| turn.accepts_end(t))
                            .max_by_key(|turn| turn.start);
                        if let Some(turn) = candidate {
                            turn.end = Some(t);
                            turn.provisional = false;
                        }
                    }
                    let seen = last_seen.get(session_id).copied();
                    last_seen.insert(session_id.clone(), seen.map_or(t, |s| s.max(t)));
                }
                EventPayload::MilestoneProposed {
                    milestone_id,
                    origin,
                    key,
                    title,
                    description,
                    required_trust,
                } => {
                    index.insert(milestone_id.clone(), view.milestones.len());
                    view.milestones.push(MilestoneView {
                        milestone_id: milestone_id.clone(),
                        key: key.clone(),
                        origin: *origin,
                        title: title.clone(),
                        description: description.clone(),
                        required_trust: *required_trust,
                        state: MilestoneState::Proposed,
                        trust: None,
                        evidence: Vec::new(),
                        reached_at: None,
                        counted_at: None,
                        split: None,
                        pending_confirmation: false,
                    });
                }
                EventPayload::MilestoneAccepted {
                    milestone_id,
                    required_trust,
                } => {
                    if let Some(m) = index.get(milestone_id).map(|&i| &mut view.milestones[i]) {
                        m.state = MilestoneState::Accepted;
                        if let Some(required) = required_trust {
                            m.required_trust = *required;
                        }
                    }
                }
                EventPayload::MilestoneRejected { milestone_id, .. } => {
                    if let Some(m) = index.get(milestone_id).map(|&i| &mut view.milestones[i]) {
                        m.state = MilestoneState::Rejected;
                    }
                }
                EventPayload::MilestoneReached {
                    milestone_id,
                    trust,
                    evidence,
                    ..
                }
                | EventPayload::MilestoneVerified {
                    milestone_id,
                    trust,
                    evidence,
                    ..
                } => {
                    if let Some(m) = index.get(milestone_id).map(|&i| &mut view.milestones[i]) {
                        let verified = matches!(e.payload, EventPayload::MilestoneVerified { .. });
                        if verified || m.state != MilestoneState::Verified {
                            m.state = if verified {
                                MilestoneState::Verified
                            } else {
                                MilestoneState::Reached
                            };
                        }
                        // Only a fresh claim moves the milestone to pending; a
                        // verification of an agent's claim leaves it pending (D6).
                        if let EventPayload::MilestoneReached { claimant, .. } = &e.payload {
                            m.pending_confirmation =
                                *claimant == Claimant::Agent && m.required_trust == Trust::SelfDeclared;
                        }
                        m.reached_at.get_or_insert(t);
                        m.trust = Some(m.trust.map_or(*trust, |cur| cur.max(*trust)));
                        for ev in evidence {
                            if !m.evidence.contains(ev) {
                                m.evidence.push(ev.clone());
                            }
                        }
                        if m.counted_at.is_none() && m.counts() {
                            m.counted_at = Some(t);
                        }
                    }
                }
                EventPayload::MilestoneInvalidated { milestone_id, .. } => {
                    if let Some(m) = index.get(milestone_id).map(|&i| &mut view.milestones[i]) {
                        m.state = MilestoneState::Accepted;
                        m.trust = None;
                        m.evidence.clear();
                        m.reached_at = None;
                        m.counted_at = None;
                        m.pending_confirmation = false;
                    }
                }
                // Not yet folded: see W6.a in docs/V0.2.md.
                EventPayload::MilestoneConfirmed { .. } => {}
                EventPayload::RunPaused { .. } => {
                    if pause_open.is_none() {
                        pause_open = Some(t);
                        view.status = RunStatus::Paused;
                    }
                }
                EventPayload::RunResumed {} => {
                    if let Some(p) = pause_open.take() {
                        pauses.push((p, t));
                        view.status = RunStatus::Running;
                    }
                }
                EventPayload::RunFinished { outcome, .. } => {
                    // Resetting a completed run closes it but keeps its result.
                    if view.status != RunStatus::Completed {
                        view.finished_at = Some(t);
                        view.status = match outcome {
                            RunOutcome::Completed => RunStatus::Completed,
                            RunOutcome::Abandoned => RunStatus::Abandoned,
                        };
                    }
                    view.closed |= *outcome == RunOutcome::Abandoned;
                }
            }
        }

        let end = view.finished_at.unwrap_or(now).max(start);
        let mut any_open = false;
        for list in turns.values() {
            for turn in list {
                match turn.end {
                    Some(e) => agent.push((turn.start, e)),
                    None => {
                        any_open = true;
                        // An open turn runs until the view's clock.
                        agent.push((turn.start, end));
                    }
                }
            }
        }
        if !view.is_finished() {
            view.open_turns = turns
                .values()
                .filter(|list| list.iter().any(|turn| turn.end.is_none()))
                .count() as u32;
            view.paused_since = pause_open;
            if !any_open && pause_open.is_none() {
                let last_busy = agent.iter().chain(pauses.iter()).map(|&(_, b)| b).max();
                view.idle_since = Some(last_busy.map_or(start, |b| b.max(start)).min(end));
            }
        }
        if let Some(p) = pause_open {
            pauses.push((p, end));
        }
        let agent: Vec<Interval> = agent.into_iter().map(|(a, b)| (a, b.min(end))).collect();
        let pauses: Vec<Interval> = pauses.into_iter().map(|(a, b)| (a, b.min(end))).collect();
        let clock = Clock::new(start, agent, pauses);
        view.timing = clock.at(end);
        for m in &mut view.milestones {
            m.split = m.counted_at.map(|t| clock.at(t));
        }
        Some(view)
    }

    pub fn milestone(&self, id_or_key: &str) -> Option<&MilestoneView> {
        self.milestones
            .iter()
            .find(|m| m.milestone_id == id_or_key)
            .or_else(|| {
                // Keys are only unique among live milestones.
                self.milestones
                    .iter()
                    .find(|m| m.key.as_deref() == Some(id_or_key) && m.state != MilestoneState::Rejected)
            })
    }

    /// Whether `cause_event_id` is still the claim that holds `milestone_id`:
    /// a later claim supersedes it, and an invalidation clears it.
    pub fn claim_is_current(events: &[Event], milestone_id: &str, cause_event_id: &str) -> bool {
        let mut current = None;
        for e in events {
            if let Some(reached) = Self::claim_change(e, milestone_id) {
                current = reached.then_some(e.event_id.as_str());
            }
        }
        current == Some(cause_event_id)
    }

    /// Whether `e` reaches (`Some(true)`) or invalidates (`Some(false)`) `milestone_id`.
    fn claim_change(e: &Event, milestone_id: &str) -> Option<bool> {
        match &e.payload {
            EventPayload::MilestoneReached { milestone_id: id, .. } => {
                (id.as_str() == milestone_id).then_some(true)
            }
            EventPayload::MilestoneInvalidated { milestone_id: id, .. } => {
                (id.as_str() == milestone_id).then_some(false)
            }
            _ => None,
        }
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.status, RunStatus::Completed | RunStatus::Abandoned)
    }

    /// True when every category milestone counts — the run is complete.
    pub fn category_complete(&self) -> bool {
        let mut predefined = self
            .milestones
            .iter()
            .filter(|m| m.origin == MilestoneOrigin::Predefined)
            .peekable();
        predefined.peek().is_some() && predefined.all(MilestoneView::counts)
    }

    /// When the last category milestone started counting.
    pub fn completion_time(&self) -> Option<DateTime<Utc>> {
        if !self.category_complete() {
            return None;
        }
        self.milestones
            .iter()
            .filter(|m| m.origin == MilestoneOrigin::Predefined)
            .filter_map(|m| m.counted_at)
            .max()
    }

    /// Whether `payload` is a legal next event for this run. The daemon
    /// calls this before appending; `fold` itself is tolerant so that a log
    /// written under older rules still loads.
    pub fn check(&self, payload: &EventPayload) -> Result<(), String> {
        use EventPayload as P;
        use MilestoneState as S;
        let state_of = |id: &str| {
            self.milestones
                .iter()
                .find(|m| m.milestone_id == id)
                .map(|m| m.state)
                .ok_or_else(|| format!("unknown milestone {id}"))
        };
        let is_milestone_event = matches!(
            payload,
            P::MilestoneProposed { .. }
                | P::MilestoneAccepted { .. }
                | P::MilestoneRejected { .. }
                | P::MilestoneReached { .. }
                | P::MilestoneVerified { .. }
                | P::MilestoneInvalidated { .. }
                | P::MilestoneConfirmed { .. }
        );
        let closing_completed = self.status == RunStatus::Completed
            && !self.closed
            && matches!(
                payload,
                P::RunFinished {
                    outcome: RunOutcome::Abandoned,
                    ..
                }
            );
        if self.is_finished()
            && !closing_completed
            && (is_milestone_event
                || !matches!(
                    payload,
                    P::PromptSubmitted { .. } | P::AgentTurnStarted { .. } | P::AgentTurnEnded { .. }
                ))
        {
            return Err(format!("run is {:?}; its result is frozen", self.status).to_lowercase());
        }
        match payload {
            P::RunStarted { .. } => Err("run already started".into()),
            P::DeviceJoined { .. } | P::DeviceRotated { .. } => {
                Err("device events are written by the daemon, not sent by clients".into())
            }
            P::PromptSubmitted { .. } | P::AgentTurnStarted { .. } | P::AgentTurnEnded { .. } => Ok(()),
            P::MilestoneProposed {
                milestone_id,
                title,
                key,
                ..
            } => {
                if self.milestones.iter().any(|m| &m.milestone_id == milestone_id) {
                    return Err(format!("milestone {milestone_id} already exists"));
                }
                if title.trim().is_empty() {
                    return Err("milestone title is empty".into());
                }
                if let Some(k) = key
                    && self.milestone(k).is_some_and(|m| m.key.as_deref() == Some(k))
                {
                    return Err(format!("a milestone with key {k} already exists"));
                }
                Ok(())
            }
            P::MilestoneAccepted { milestone_id, .. } | P::MilestoneRejected { milestone_id, .. } => {
                match state_of(milestone_id)? {
                    S::Proposed => Ok(()),
                    s => Err(format!("milestone is {s:?}, not proposed").to_lowercase()),
                }
            }
            P::MilestoneReached { milestone_id, .. } => match state_of(milestone_id)? {
                S::Accepted | S::Reached => Ok(()),
                s => Err(format!("cannot reach a milestone that is {s:?}").to_lowercase()),
            },
            P::MilestoneVerified { milestone_id, .. } => match state_of(milestone_id)? {
                S::Reached | S::Verified => Ok(()),
                s => Err(format!("cannot verify a milestone that is {s:?}").to_lowercase()),
            },
            P::MilestoneInvalidated { milestone_id, .. } => match state_of(milestone_id)? {
                S::Reached | S::Verified => Ok(()),
                s => Err(format!("cannot invalidate a milestone that is {s:?}").to_lowercase()),
            },
            P::MilestoneConfirmed { .. } => Err("milestone confirmation is not implemented".into()),
            P::RunPaused { .. } if self.open_turns > 0 => {
                Err("cannot pause while an agent turn is open".into())
            }
            P::RunPaused { .. } if self.status == RunStatus::Paused => Err("run is already paused".into()),
            P::RunResumed {} if self.status != RunStatus::Paused => Err("run is not paused".into()),
            P::RunPaused { .. } | P::RunResumed {} | P::RunFinished { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Claimant, SCHEMA_VERSION, hash::compute_hash};

    fn t(min: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::minutes(min)
    }

    fn log(items: Vec<(i64, EventPayload)>) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();
        let mut all = vec![(
            0,
            EventPayload::RunStarted {
                project_id: "p".into(),
                project_root: None,
                category_id: "any-percent".into(),
                category_version: 1,
                device_pubkey: None,
                start_head: None,
                start_head_committed_at: None,
                ruleset_digest: None,
                disclosure_commitment: None,
            },
        )];
        all.extend(items);
        for (i, (min, payload)) in all.into_iter().enumerate() {
            let mut e = Event {
                event_id: format!("e{i}"),
                schema_version: SCHEMA_VERSION,
                run_id: "r".into(),
                seq: i as u64 + 1,
                device_id: "d".into(),
                occurred_at: t(min),
                recorded_at: t(min),
                payload,
                prev_hash: out.last().map(|p| p.hash.clone()),
                hash: String::new(),
                sig: None,
            };
            e.hash = compute_hash(&e);
            out.push(e);
        }
        out
    }

    fn turn(start: i64, end: i64, s: &str) -> Vec<(i64, EventPayload)> {
        vec![
            (start, EventPayload::AgentTurnStarted { session_id: s.into() }),
            (end, EventPayload::AgentTurnEnded { session_id: s.into() }),
        ]
    }

    fn propose(id: &str, required: Trust) -> Vec<(i64, EventPayload)> {
        vec![
            (
                0,
                EventPayload::MilestoneProposed {
                    milestone_id: id.into(),
                    origin: MilestoneOrigin::Predefined,
                    key: Some(id.into()),
                    title: id.into(),
                    description: None,
                    required_trust: required,
                },
            ),
            (
                0,
                EventPayload::MilestoneAccepted {
                    milestone_id: id.into(),
                    required_trust: None,
                },
            ),
        ]
    }

    #[test]
    fn concurrent_sessions_count_agent_time_once() {
        let mut items = turn(0, 10, "a");
        items.extend(turn(5, 20, "b"));
        let v = RunView::fold(&log(items), t(20)).unwrap();
        assert_eq!(v.timing.agent_ms, 20 * 60_000);
        assert_eq!(v.timing.wall_ms, 20 * 60_000);
        assert_eq!(v.timing.human_ms, 0);
    }

    #[test]
    fn human_gaps_are_capped_and_pauses_excluded() {
        let mut items = turn(0, 10, "a");
        items.extend(turn(50, 60, "a")); // 40 min gap → 15 min human
        items.push((60, EventPayload::RunPaused { reason: None }));
        items.push((120, EventPayload::RunResumed {}));
        items.push((
            125,
            EventPayload::AgentTurnStarted {
                session_id: "a".into(),
            },
        ));
        let v = RunView::fold(&log(items), t(130)).unwrap();
        assert_eq!(v.timing.wall_ms, 130 * 60_000);
        assert_eq!(v.timing.timed_ms, 70 * 60_000);
        assert_eq!(v.timing.agent_ms, 25 * 60_000);
        assert_eq!(v.timing.human_ms, (15 + 5) * 60_000);
    }

    #[test]
    fn milestone_counts_only_at_required_trust() {
        let mut items = propose("demo", Trust::EvidenceVerified);
        items.extend(turn(0, 30, "a"));
        items.push((
            20,
            EventPayload::MilestoneReached {
                milestone_id: "demo".into(),
                trust: Trust::AgentClaimed,
                evidence: vec![],
                claimant: Claimant::User,
            },
        ));
        let v = RunView::fold(&log(items.clone()), t(30)).unwrap();
        assert!(!v.milestone("demo").unwrap().counts());
        assert!(!v.category_complete());

        items.push((
            25,
            EventPayload::MilestoneVerified {
                milestone_id: "demo".into(),
                trust: Trust::EvidenceVerified,
                verifier: "git".into(),
                evidence: vec![Evidence::GitCommit { sha: "abc".into() }],
            },
        ));
        let v = RunView::fold(&log(items), t(30)).unwrap();
        let m = v.milestone("demo").unwrap();
        assert!(m.counts());
        assert_eq!(m.split.unwrap().agent_ms, 25 * 60_000);
        assert_eq!(v.completion_time(), Some(t(25)));
    }

    #[test]
    fn check_enforces_milestone_lifecycle() {
        let mut items = propose("demo", Trust::SelfDeclared);
        items.push((
            1,
            EventPayload::MilestoneProposed {
                milestone_id: "x".into(),
                origin: MilestoneOrigin::AgentGenerated,
                key: None,
                title: "agent idea".into(),
                description: None,
                required_trust: Trust::AgentClaimed,
            },
        ));
        let v = RunView::fold(&log(items), t(2)).unwrap();
        let reach = |id: &str| EventPayload::MilestoneReached {
            milestone_id: id.into(),
            trust: Trust::AgentClaimed,
            evidence: vec![],
            claimant: Claimant::User,
        };
        assert!(v.check(&reach("demo")).is_ok());
        assert!(
            v.check(&reach("x")).is_err(),
            "unaccepted proposals cannot be reached"
        );
        assert!(v.check(&reach("nope")).is_err());
        assert!(v.check(&EventPayload::RunResumed {}).is_err());
    }

    #[test]
    fn finished_runs_are_frozen() {
        let mut items = propose("demo", Trust::SelfDeclared);
        items.push((
            5,
            EventPayload::RunFinished {
                outcome: RunOutcome::Completed,
                reason: None,
            },
        ));
        let v = RunView::fold(&log(items), t(100)).unwrap();
        assert_eq!(v.timing.wall_ms, 5 * 60_000, "timer stops at finish");
        assert!(
            v.check(&EventPayload::MilestoneInvalidated {
                milestone_id: "demo".into(),
                reason: "x".into()
            })
            .is_err()
        );
        assert!(
            v.check(&EventPayload::AgentTurnStarted {
                session_id: "a".into()
            })
            .is_ok()
        );
    }
}
