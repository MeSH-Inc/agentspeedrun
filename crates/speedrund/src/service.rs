//! Business rules on top of the store: run lifecycle, validation,
//! verification and completion.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use speedrun_core::{
    Category, Event, EventPayload, Evidence, MilestoneOrigin, MilestoneState, RunOutcome, RunStatus, RunView,
    Trust,
    api::{
        AppendRequest, AppendResponse, ChainReport, ExportResponse, ProjectAppendRequest, PromptCommitment,
        PromptRequest, PromptResponse, RotateResponse,
    },
    category,
    hash::verify_stored_chain,
    live::{LIVE_PROTOCOL_VERSION, LiveRun, LiveSnapshot, LiveSource, LiveSourceKind, SnapshotError},
    redact::redact,
};

use crate::clock::ClockWindow;
use crate::store::{Reader, Store, Tx, UnreadableRow};
use speedrun_verify as verify;

/// A verification that finishes within this long of the claim is dated at
/// the claim (verifier latency does not count against the runner); later
/// ones, such as spooled claims replayed hours afterwards, are dated when
/// the evidence was actually checked.
const VERIFY_GRACE: Duration = Duration::minutes(10);
/// The daemon's tolerance for client clocks, in one place.
const CLOCK: ClockWindow = ClockWindow::DAEMON;
const MAX_ID_LEN: usize = 128;

#[derive(Debug)]
pub enum Error {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    Internal(anyhow::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<crate::provenance::Refusal> for Error {
    fn from(refusal: crate::provenance::Refusal) -> Self {
        Self::Forbidden(refusal.reason().to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// An accepted append, plus the evidence check it calls for. The caller
/// runs `job` without holding the service (it may touch the network) and
/// hands the outcome to `finish_verification`.
pub struct Appended {
    pub response: AppendResponse,
    pub job: Option<verify::Job>,
}

/// Turn rows that could not be decoded into snapshot errors.
fn snapshot_errors(rows: impl IntoIterator<Item = UnreadableRow>) -> Vec<SnapshotError> {
    rows.into_iter()
        .map(|row| SnapshotError {
            run_id: row.run_id,
            event_id: row.event_id,
            message: row.error,
        })
        .collect()
}

/// Cached personal-best candidates for one project@category@version, plus
/// the rows those logs held that could not be decoded when it was filled.
struct PbGroup {
    runs: Vec<RunView>,
    unreadable: Vec<UnreadableRow>,
}

pub struct Service {
    store: Store,
    categories: Vec<Category>,
    /// The daemon's own `data_dir_id` (the value `/v1/health` reports), which
    /// every live snapshot names as its source.
    source_id: String,
    /// Completed runs per project@category@version, fastest first by `timed_ms`.
    /// A live run's PB is taken from its own project's entry only.
    /// Cleared whenever any run finishes.
    pb_cache: HashMap<(String, String, u32), PbGroup>,
}

fn check_id(name: &str, id: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID_LEN || id.chars().any(char::is_control) {
        return Err(Error::BadRequest(format!(
            "{name} must be 1-{MAX_ID_LEN} printable chars"
        )));
    }
    Ok(())
}

fn fold(tx: &Tx<'_>, run_id: &str) -> Result<RunView> {
    let events = tx.reader().run_events(run_id)?;
    let view = RunView::fold(&events, Utc::now()).ok_or_else(|| Error::NotFound(format!("run {run_id}")))?;
    attach_root(&tx.reader(), view)
}

/// Attach the local project root to a folded view. `RunView::fold` is pure
/// and may read a root an old log stored; the store is the only place that
/// knows where a new run's root lives.
fn attach_root(reader: &Reader<'_>, mut view: RunView) -> Result<RunView> {
    view.project_root = reader.run_root(&view.run_id)?;
    Ok(view)
}

/// The run a new event for `project_id` joins: the project's newest run,
/// unless it was abandoned, which closed it. `None` means the write starts a
/// fresh run. This is the one place write paths decide which run they target.
fn open_run_id(reader: &Reader<'_>, project_id: &str) -> Result<Option<String>> {
    match reader.latest_run(project_id)? {
        Some(run_id) => {
            let closed = reader.run_events(&run_id)?.into_iter().any(|e| {
                matches!(
                    e.payload,
                    EventPayload::RunFinished {
                        outcome: RunOutcome::Abandoned,
                        ..
                    }
                )
            });
            Ok((!closed).then_some(run_id))
        }
        None => Ok(None),
    }
}

/// The event that proposed `milestone_id` in this log, if any. A retried
/// proposal names the same milestone, so it must become a no-op.
fn proposal_event<'a>(events: &'a [Event], milestone_id: &str) -> Option<&'a Event> {
    events.iter().find(|e| match &e.payload {
        EventPayload::MilestoneProposed { milestone_id: id, .. } => id == milestone_id,
        _ => false,
    })
}

/// Point a milestone event at the id the run already stores for the milestone
/// it names. A client may give a predefined milestone's category key; the
/// namespaced id is `start_run`'s rule, so it is looked up through the folded
/// run rather than rebuilt here. An unknown name is left for `check` to reject.
fn resolve_milestone_id(run: &RunView, payload: EventPayload) -> EventPayload {
    let id = match &payload {
        EventPayload::MilestoneProposed { milestone_id, .. }
        | EventPayload::MilestoneAccepted { milestone_id, .. }
        | EventPayload::MilestoneRejected { milestone_id, .. }
        | EventPayload::MilestoneReached { milestone_id, .. }
        | EventPayload::MilestoneVerified { milestone_id, .. }
        | EventPayload::MilestoneInvalidated { milestone_id, .. }
        | EventPayload::MilestoneConfirmed { milestone_id } => milestone_id.clone(),
        _ => return payload,
    };
    let resolved = match run.milestone(&id) {
        Some(m) => m.milestone_id.clone(),
        None => return payload,
    };
    match payload {
        EventPayload::MilestoneProposed {
            origin,
            key,
            title,
            description,
            required_trust,
            ..
        } => EventPayload::MilestoneProposed {
            milestone_id: resolved,
            origin,
            key,
            title,
            description,
            required_trust,
        },
        EventPayload::MilestoneAccepted { required_trust, .. } => EventPayload::MilestoneAccepted {
            milestone_id: resolved,
            required_trust,
        },
        EventPayload::MilestoneRejected { reason, .. } => EventPayload::MilestoneRejected {
            milestone_id: resolved,
            reason,
        },
        EventPayload::MilestoneReached {
            trust,
            evidence,
            claimant,
            ..
        } => EventPayload::MilestoneReached {
            milestone_id: resolved,
            trust,
            evidence,
            claimant,
        },
        EventPayload::MilestoneConfirmed { .. } => EventPayload::MilestoneConfirmed {
            milestone_id: resolved,
        },
        EventPayload::MilestoneVerified {
            trust,
            verifier,
            evidence,
            ..
        } => EventPayload::MilestoneVerified {
            milestone_id: resolved,
            trust,
            verifier,
            evidence,
        },
        EventPayload::MilestoneInvalidated { reason, .. } => EventPayload::MilestoneInvalidated {
            milestone_id: resolved,
            reason,
        },
        other => other,
    }
}

impl Service {
    /// `source_id` names the daemon in every live snapshot: its `data_dir_id`,
    /// which `/v1/health` reports and `Health` owns, passed in rather than
    /// computed again.
    pub fn new(store: Store, categories: Vec<Category>, source_id: String) -> Self {
        Self {
            store,
            categories,
            source_id,
            pb_cache: HashMap::new(),
        }
    }

    pub fn categories(&self) -> &[Category] {
        &self.categories
    }

    /// Replace this device's signing key. Runs that have not written since
    /// the rotation record a DeviceRotated before their next event.
    pub fn rotate_device(&mut self) -> Result<RotateResponse> {
        self.store.rotate_key().map_err(Error::Internal)
    }

    /// Record a prompt. The first prompt for a project (or the first after a
    /// reset) starts a run and seeds the category's milestones.
    pub fn record_prompt(&mut self, req: PromptRequest) -> Result<PromptResponse> {
        check_id("event_id", &req.event_id)?;
        check_id("project_id", &req.project_id)?;
        check_id("session_id", &req.session_id)?;
        let categories = &self.categories;
        self.store.transaction(|tx| {
            if let Some(existing) = tx.reader().event(&req.event_id)? {
                return match existing.payload {
                    EventPayload::PromptSubmitted { .. } => Ok(PromptResponse {
                        run_id: existing.run_id.clone(),
                        run_started: false,
                        event: existing,
                    }),
                    _ => Err(Error::Conflict(format!(
                        "event_id {} is not a prompt",
                        req.event_id
                    ))),
                };
            }
            let at = CLOCK.clamp(req.occurred_at, Utc::now());
            let current = open_run_id(&tx.reader(), &req.project_id)?;
            let (run_id, run_started) = match current {
                Some(run_id) => (run_id, false),
                None => {
                    // `None` is the client asking for the daemon's latest version;
                    // any other value must name a version this build ships, or no
                    // run is started at all.
                    let cat = category::find(categories, &req.category_id, req.category_version).ok_or_else(
                        || match req.category_version {
                            Some(version) => Error::BadRequest(format!(
                                "unknown category {} version {version}",
                                req.category_id
                            )),
                            None => Error::BadRequest(format!("unknown category {}", req.category_id)),
                        },
                    )?;
                    (start_run(tx, &req, cat, at)?, true)
                }
            };
            let (content, redacted) = redact(&req.content);
            let event = tx.insert_prompt(
                &run_id,
                &req.event_id,
                at,
                EventPayload::PromptSubmitted {
                    session_id: req.session_id.clone(),
                    host: req.host.clone(),
                    model: req.model.clone(),
                    content_digest: String::new(),
                    length: content.chars().count() as u64,
                    redacted,
                },
                &content,
            )?;
            if req.starts_turn {
                tx.insert(
                    &run_id,
                    &format!("{}:turn", req.event_id),
                    at,
                    EventPayload::AgentTurnStarted {
                        session_id: req.session_id.clone(),
                    },
                )?;
            }
            Ok(PromptResponse {
                run_id,
                run_started,
                event,
            })
        })
    }

    /// Append, verify inline and finish, as the HTTP layer does across threads.
    #[cfg(test)]
    pub fn append_verified(
        &mut self,
        req: AppendRequest,
        verifier: &verify::Verifier,
    ) -> Result<AppendResponse> {
        let Appended { mut response, job } = self.append(req)?;
        if let Some(job) = job {
            let outcome = verifier.verify(&job);
            response.derived.extend(self.finish_verification(&job, &outcome)?);
            response.verification = Some(outcome.report());
        }
        Ok(response)
    }

    /// Append a client event to an existing run, then derive any completion
    /// that follows from it. Evidence is not checked here: see `Appended`.
    pub fn append(&mut self, req: AppendRequest) -> Result<Appended> {
        check_id("event_id", &req.event_id)?;
        match &req.payload {
            EventPayload::RunStarted { .. } => {
                return Err(Error::BadRequest("runs start by recording a prompt".into()));
            }
            EventPayload::MilestoneVerified { .. } => {
                return Err(Error::BadRequest("only the daemon's verifiers may verify".into()));
            }
            EventPayload::MilestoneReached { trust, .. } if *trust >= Trust::EvidenceVerified => {
                return Err(Error::BadRequest(
                    "claims are self_declared or agent_claimed; attach evidence to get verified".into(),
                ));
            }
            EventPayload::MilestoneProposed {
                origin: MilestoneOrigin::Predefined,
                ..
            } => {
                return Err(Error::BadRequest(
                    "predefined milestones come from the category".into(),
                ));
            }
            EventPayload::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            } => {
                return Err(Error::BadRequest(
                    "runs complete when every category milestone counts".into(),
                ));
            }
            _ => {}
        }
        if let EventPayload::MilestoneProposed { milestone_id, .. } = &req.payload {
            check_id("milestone_id", milestone_id)?;
        }

        let (event, inserted) = self.store.transaction(|tx| {
            let events = tx.reader().run_events(&req.run_id)?;
            let run = RunView::fold(&events, Utc::now());
            // A client may name a predefined milestone by its category key. The
            // namespaced id is `start_run`'s rule; resolving through the folded
            // run keeps this path from knowing it, and makes a bare-key retry
            // compare equal to the namespaced event already stored.
            let payload = match &run {
                Some(run) => resolve_milestone_id(run, req.payload.clone()),
                None => req.payload.clone(),
            };
            if let Some(existing) = tx.reader().event(&req.event_id)? {
                let same = existing.run_id == req.run_id
                    && (existing.payload == payload || existing.payload == req.payload);
                if !same {
                    return Err(Error::Conflict(format!(
                        "event_id {} already used for a different event",
                        req.event_id
                    )));
                }
                return Ok((existing, false));
            }
            if let EventPayload::MilestoneProposed { milestone_id, .. } = &payload
                && let Some(existing) = proposal_event(&events, milestone_id)
            {
                // A retained proposal is a retry, not a conflict: it may have
                // been spooled while the daemon was unreachable, and flush
                // must deliver it as a no-op instead of rejecting it.
                return Ok((existing.clone(), false));
            }
            let run = run.ok_or_else(|| Error::NotFound(format!("run {}", req.run_id)))?;
            run.check(&payload).map_err(Error::Conflict)?;
            let at = CLOCK.clamp(req.occurred_at, Utc::now());
            let e = tx.insert(&req.run_id, &req.event_id, at, payload)?;
            Ok((e, true))
        })?;

        // Derived steps run even on retries; their event ids make them
        // idempotent, and a retried claim re-checks its evidence.
        let job = self.verification_job(&event)?;
        let derived: Vec<Event> = self.complete_if_done(&event.run_id)?.into_iter().collect();
        if std::iter::once(&event)
            .chain(&derived)
            .any(|e| matches!(e.payload, EventPayload::RunFinished { .. }))
        {
            self.pb_cache.clear();
        }
        Ok(Appended {
            response: AppendResponse {
                event,
                inserted,
                derived,
                verification: None,
            },
            job,
        })
    }

    /// Append to the project's active run.
    pub fn append_to_project(
        &mut self,
        project_id: &str,
        req: ProjectAppendRequest,
        from_spool: bool,
    ) -> Result<Appended> {
        // A retry must land in the run the original went to, even if a newer run exists.
        let run_id = match self.store.reader().event(&req.event_id)? {
            Some(existing) => existing.run_id,
            None => open_run_id(&self.store.reader(), project_id)?.ok_or_else(|| {
                Error::NotFound(format!(
                    "no active run for project {project_id}; send a prompt first"
                ))
            })?,
        };
        self.check_occurred_at(&req.event_id, &run_id, req.occurred_at, from_spool)?;
        self.append(AppendRequest {
            event_id: req.event_id,
            run_id,
            occurred_at: req.occurred_at,
            payload: req.payload,
        })
    }

    /// Refuse a write dated outside the run's clock window: before the run
    /// started, or (unless it is a spool replay) too far behind the log.
    /// A retry of an event already recorded is not judged again: the write
    /// answers it with the original, as it did before the bound existed.
    ///
    /// The HTTP layer calls this before the write, inside the same service
    /// lock, so the check and the append cannot interleave with another write.
    pub fn check_occurred_at(
        &self,
        event_id: &str,
        run_id: &str,
        occurred_at: Option<DateTime<Utc>>,
        from_spool: bool,
    ) -> Result<()> {
        if self.store.reader().event(event_id)?.is_some() {
            return Ok(());
        }
        match self.store.reader().run_bounds(run_id)? {
            Some((start, last)) => CLOCK
                .admit(occurred_at, start, last, Utc::now(), from_spool)
                .map_err(Error::BadRequest),
            None => Ok(()),
        }
    }

    /// Refuse a prompt dated outside the window of the run it will join. A
    /// prompt that starts a fresh run has no earlier events to be behind.
    pub fn check_prompt_occurred_at(
        &self,
        event_id: &str,
        project_id: &str,
        occurred_at: Option<DateTime<Utc>>,
        from_spool: bool,
    ) -> Result<()> {
        match open_run_id(&self.store.reader(), project_id)? {
            Some(run_id) => self.check_occurred_at(event_id, &run_id, occurred_at, from_spool),
            None => Ok(()),
        }
    }

    /// The evidence check a claim calls for, if any.
    fn verification_job(&self, cause: &Event) -> Result<Option<verify::Job>> {
        let EventPayload::MilestoneReached {
            milestone_id,
            evidence,
            ..
        } = &cause.payload
        else {
            return Ok(None);
        };
        if !verify::Job::has_checkable_evidence(evidence) {
            return Ok(None);
        }
        let events = self.store.reader().run_events(&cause.run_id)?;
        let Some(run) = self.rooted_view(&events, Utc::now())? else {
            return Ok(None);
        };
        let Some(m) = run
            .milestone(milestone_id)
            .filter(|m| m.state == MilestoneState::Reached)
        else {
            return Ok(None); // Already verified, or invalidated since.
        };
        Ok(Some(verify::Job {
            run_id: cause.run_id.clone(),
            cause_event_id: cause.event_id.clone(),
            milestone_id: milestone_id.clone(),
            key: m.key.clone(),
            evidence: evidence.clone(),
            project_root: run.project_root.clone(),
            run_started_at: run.started_at,
            claimed_at: cause.occurred_at,
            deployed_url: deployed_url(&run),
        }))
    }

    /// Record a passed verification, and the completion it may cause.
    pub fn finish_verification(
        &mut self,
        job: &verify::Job,
        outcome: &verify::Outcome,
    ) -> Result<Vec<Event>> {
        if !outcome.verified() {
            return Ok(Vec::new());
        }
        // A job is only good while its cause is still the milestone's current
        // claim; invalidation or a newer claim makes the evidence stale.
        let events = self.store.reader().run_events(&job.run_id)?;
        if !RunView::claim_is_current(&events, &job.milestone_id, &job.cause_event_id) {
            return Ok(Vec::new());
        }
        let now = Utc::now();
        let at = if now - job.claimed_at <= VERIFY_GRACE {
            job.claimed_at
        } else {
            now
        };
        let payload = EventPayload::MilestoneVerified {
            milestone_id: job.milestone_id.clone(),
            trust: Trust::EvidenceVerified,
            verifier: outcome.verifiers.join("+"),
            evidence: job.evidence.clone(),
        };
        let mut out: Vec<Event> = self
            .insert_derived(
                &job.run_id,
                &format!("{}:verify", job.cause_event_id),
                at,
                payload,
            )?
            .into_iter()
            .collect();
        out.extend(self.complete_if_done(&job.run_id)?);
        if out
            .iter()
            .any(|e| matches!(e.payload, EventPayload::RunFinished { .. }))
        {
            self.pb_cache.clear();
        }
        Ok(out)
    }

    fn complete_if_done(&mut self, run_id: &str) -> Result<Option<Event>> {
        let events = self.store.reader().run_events(run_id)?;
        let Some(run) = RunView::fold(&events, Utc::now()) else {
            return Ok(None);
        };
        let Some(at) = run.completion_time().filter(|_| !run.is_finished()) else {
            return Ok(None);
        };
        let payload = EventPayload::RunFinished {
            outcome: RunOutcome::Completed,
            reason: Some(format!("all {} milestones count", run.category_id)),
        };
        self.insert_derived(run_id, &format!("{run_id}:completed"), at, payload)
    }

    /// Insert a daemon-derived event if it is not already present and still valid.
    fn insert_derived(
        &mut self,
        run_id: &str,
        event_id: &str,
        at: DateTime<Utc>,
        payload: EventPayload,
    ) -> Result<Option<Event>> {
        self.store.transaction(|tx| {
            if tx.reader().event(event_id)?.is_some() {
                return Ok(None);
            }
            let run = fold(tx, run_id)?;
            let payload = resolve_milestone_id(&run, payload);
            if run.check(&payload).is_err() {
                return Ok(None);
            }
            Ok(Some(tx.insert(run_id, event_id, at, payload)?))
        })
    }

    /// Snapshot for live displays: the newest open run of every project
    /// active within `window`.
    pub fn live(&mut self, window: Duration) -> Result<LiveSnapshot> {
        let as_of = Utc::now();
        let reader = self.store.reader();
        let cursor = reader.cursor()?;
        let mut views = Vec::new();
        for run_id in reader.latest_runs()? {
            let log = reader.run_log(&run_id)?;
            let Some(view) = self.rooted_view(&log.events, as_of)? else {
                continue;
            };
            if !view.closed && view.last_event_at >= as_of - window {
                views.push((view, log.unreadable));
            }
        }
        let mut runs = Vec::with_capacity(views.len());
        for (view, unreadable) in views {
            let name = category::find(&self.categories, &view.category_id, Some(view.category_version))
                .map_or_else(|| view.category_id.clone(), |c| c.name.clone());
            let group = self.completed_runs(&view)?;
            let pb = group.runs.iter().find(|r| r.run_id != view.run_id);
            let mut run = LiveRun::from_view(&view, &name, as_of, pb);
            run.errors = snapshot_errors(unreadable);
            for row in &group.unreadable {
                if row.run_id != view.run_id {
                    run.errors.push(SnapshotError {
                        run_id: row.run_id.clone(),
                        event_id: row.event_id.clone(),
                        message: row.error.clone(),
                    });
                }
            }
            runs.push(run);
        }
        runs.sort_by_key(|r| (r.finished_at.is_some(), std::cmp::Reverse(r.last_event_at)));
        Ok(LiveSnapshot {
            protocol: LIVE_PROTOCOL_VERSION,
            source: LiveSource {
                kind: LiveSourceKind::Local,
                id: self.source_id.clone(),
            },
            as_of,
            cursor,
            runs,
        })
    }

    /// The fastest completed runs of `project_id` in `category_id`@`version`,
    /// fastest first. Choosing the PB here — rather than from the whole
    /// category — is what keeps a project's comparison to its own results.
    fn completed_runs(&mut self, view: &RunView) -> Result<&PbGroup> {
        let project_id = &view.project_id;
        let category_id = &view.category_id;
        let version = view.category_version;
        let key = (project_id.clone(), category_id.clone(), version);
        if !self.pb_cache.contains_key(&key) {
            let reader = self.store.reader();
            let now = Utc::now();
            let mut done = Vec::new();
            let mut unreadable = Vec::new();
            for run_id in reader.runs_in_category_for_project(project_id, category_id, version)? {
                let log = reader.run_log(&run_id)?;
                unreadable.extend(log.unreadable);
                if let Some(v) = self.rooted_view(&log.events, now)?
                    && v.status == RunStatus::Completed
                {
                    done.push(v);
                }
            }
            done.sort_by_key(|v| v.timing.timed_ms);
            // Two is enough: the best, and the runner-up for when the best is the run being shown.
            done.truncate(2);
            self.pb_cache.insert(
                key.clone(),
                PbGroup {
                    runs: done,
                    unreadable,
                },
            );
        }
        Ok(&self.pb_cache[&key])
    }

    /// Fold events and attach the root the store keeps for the run. This is
    /// the one path callers use to turn a log into a view with local state.
    fn rooted_view(&self, events: &[Event], now: DateTime<Utc>) -> Result<Option<RunView>> {
        let reader = self.store.reader();
        let Some(view) = RunView::fold(events, now) else {
            return Ok(None);
        };
        attach_root(&reader, view).map(Some)
    }

    pub fn active_run(&self, project_id: &str) -> Result<Option<RunView>> {
        let r = self.store.reader();
        let Some(run_id) = r.latest_run(project_id)? else {
            return Ok(None);
        };
        let events = r.run_events(&run_id)?;
        Ok(self.rooted_view(&events, Utc::now())?.filter(|v| !v.closed))
    }

    pub fn run(&self, run_id: &str) -> Result<RunView> {
        let events = self.store.reader().run_events(run_id)?;
        self.rooted_view(&events, Utc::now())?
            .ok_or_else(|| Error::NotFound(format!("run {run_id}")))
    }

    pub fn events(&self, run_id: &str) -> Result<Vec<Event>> {
        let events = self.store.reader().run_events(run_id)?;
        if events.is_empty() {
            return Err(Error::NotFound(format!("run {run_id}")));
        }
        Ok(events)
    }

    pub fn verify_chain(&self, run_id: &str) -> Result<ChainReport> {
        let events = self.events(run_id)?;
        let bodies = self.store.reader().run_bodies(run_id)?;
        let chain = verify_stored_chain(&bodies);
        let signatures = speedrun_core::sign::verify_event_signatures(&events);
        let error = chain.err().map(|e| e.to_string()).or(signatures.error);
        Ok(ChainReport {
            run_id: run_id.to_string(),
            events: events.len() as u64,
            ok: error.is_none(),
            error,
            head_hash: events.last().map(|e| e.hash.clone()),
            signed: signatures.signed,
            unsigned: signatures.unsigned,
        })
    }

    /// A tier 2 publication of a run: one commitment per prompt and no
    /// prompt text. The disclosure key stays on this machine.
    pub fn export(&self, run_id: &str, tier: u8) -> Result<ExportResponse> {
        if tier != 2 {
            return Err(Error::BadRequest(format!(
                "tier {tier} is not supported; only tier 2 is defined"
            )));
        }
        let events = self.store.reader().run_events(run_id)?;
        if events.is_empty() {
            return Err(Error::NotFound(format!("run {run_id}")));
        }
        let disclosure_commitment = events
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::RunStarted {
                    disclosure_commitment,
                    ..
                } => disclosure_commitment.clone(),
                _ => None,
            })
            .ok_or_else(|| Error::NotFound(format!("run {run_id} has no disclosure commitment")))?;
        let key = self.store.disclosure_key(run_id).map_err(Error::Internal)?;
        let reader = self.store.reader();
        let mut prompts = Vec::new();
        for event in &events {
            if let EventPayload::PromptSubmitted { length, redacted, .. } = &event.payload
                && let Some(text) = reader.prompt_text(&event.event_id)?
            {
                prompts.push(PromptCommitment {
                    event_id: event.event_id.clone(),
                    commitment: speedrun_core::digest::text_digest(&key, &text),
                    length: *length,
                    redacted: *redacted,
                });
            }
        }
        Ok(ExportResponse {
            run_id: run_id.to_string(),
            tier,
            disclosure_commitment,
            prompts,
        })
    }
}

/// The URL the run's `deployed` milestone was verified with, if it was.
fn deployed_url(run: &RunView) -> Option<String> {
    let deployed = run
        .milestones
        .iter()
        .find(|m| m.key.as_deref() == Some("deployed") && m.trust == Some(Trust::EvidenceVerified))?;
    deployed.evidence.iter().find_map(|e| match e {
        Evidence::Url { url } => Some(url.clone()),
        _ => None,
    })
}

fn start_run(tx: &mut Tx<'_>, req: &PromptRequest, cat: &Category, at: DateTime<Utc>) -> Result<String> {
    let run_id = speedrun_core::new_id();
    let base = &req.event_id;
    let (start_head, start_head_committed_at) = match req.project_root.as_deref() {
        Some(root) => match speedrun_verify::git::head(root) {
            Ok(Some(head)) => (Some(head.sha), Some(head.committed_at)),
            Ok(None) => (None, None),
            Err(e) => return Err(Error::Internal(anyhow::Error::msg(e))),
        },
        None => (None, None),
    };
    let ruleset_digest = category::digest(cat).map_err(|e| Error::Internal(e.into()))?;
    let disclosure_commitment = tx.disclose(&run_id).map_err(Error::Internal)?;
    tx.insert(
        &run_id,
        &format!("{base}:run"),
        at,
        EventPayload::RunStarted {
            project_id: req.project_id.clone(),
            project_root: None,
            category_id: cat.id.clone(),
            category_version: cat.version,
            device_pubkey: Some(tx.device_pubkey()),
            start_head,
            start_head_committed_at,
            ruleset_digest: Some(ruleset_digest),
            disclosure_commitment: Some(disclosure_commitment),
        },
    )?;
    tx.record_run_root(&run_id, req.project_root.as_deref())?;
    for m in &cat.milestones {
        // A predefined milestone's log id names the ruleset it came from, so a
        // later category edit (or a key shared with another category) cannot
        // silently change what this run's milestones mean.
        let milestone_id = format!("{}@{}/{}", cat.id, cat.version, m.key);
        tx.insert(
            &run_id,
            &format!("{base}:m:{}", m.key),
            at,
            EventPayload::MilestoneProposed {
                milestone_id: milestone_id.clone(),
                origin: MilestoneOrigin::Predefined,
                key: Some(m.key.clone()),
                title: m.title.clone(),
                description: Some(m.description.clone()),
                required_trust: m.required_trust,
            },
        )?;
        tx.insert(
            &run_id,
            &format!("{base}:m:{}:accept", m.key),
            at,
            EventPayload::MilestoneAccepted {
                milestone_id,
                required_trust: None,
            },
        )?;
    }
    Ok(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use speedrun_core::{Claimant, Evidence};

    fn svc() -> (Service, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("device.key"), "07".repeat(32)).unwrap();
        (open_in(&dir), dir)
    }

    fn prompt(id: &str) -> PromptRequest {
        PromptRequest {
            event_id: id.into(),
            project_id: "proj".into(),
            project_root: None,
            category_id: "any-percent".into(),
            category_version: None,
            session_id: "s1".into(),
            host: "test".into(),
            model: None,
            content: "build it. key: sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            occurred_at: None,
            starts_turn: true,
        }
    }

    fn append(s: &mut Service, run: &str, id: &str, payload: EventPayload) -> Result<AppendResponse> {
        let req = AppendRequest {
            event_id: id.into(),
            run_id: run.into(),
            occurred_at: None,
            payload,
        };
        s.append_verified(req, &verify::Verifier::default())
    }

    #[test]
    fn first_prompt_starts_run_with_category_milestones() {
        let (mut s, _d) = svc();
        let r = s.record_prompt(prompt("p1")).unwrap();
        assert!(r.run_started);
        let run = s.run(&r.run_id).unwrap();
        assert_eq!(run.prompt_count, 1);
        assert_eq!(run.milestones.len(), 2);
        assert!(run.milestones.iter().all(|m| m.state == MilestoneState::Accepted));
        let EventPayload::PromptSubmitted { redacted, .. } = &r.event.payload else {
            panic!()
        };
        assert!(*redacted);
        assert!(!serde_json::to_string(&r.event).unwrap().contains("sk-ant"));

        let r2 = s.record_prompt(prompt("p2")).unwrap();
        assert!(!r2.run_started);
        assert_eq!(r2.run_id, r.run_id);
        assert!(s.verify_chain(&r.run_id).unwrap().ok);
    }

    #[test]
    fn retries_are_idempotent() {
        let (mut s, _d) = svc();
        let a = s.record_prompt(prompt("p1")).unwrap();
        let b = s.record_prompt(prompt("p1")).unwrap();
        assert_eq!(a.event, b.event);
        assert_eq!(s.run(&a.run_id).unwrap().prompt_count, 1);

        let payload = EventPayload::AgentTurnEnded {
            session_id: "s1".into(),
        };
        assert!(append(&mut s, &a.run_id, "t1", payload.clone()).unwrap().inserted);
        assert!(!append(&mut s, &a.run_id, "t1", payload).unwrap().inserted);
        let other = EventPayload::RunPaused { reason: None };
        assert!(matches!(
            append(&mut s, &a.run_id, "t1", other),
            Err(Error::Conflict(_))
        ));
    }

    #[test]
    fn clients_cannot_self_verify_or_force_completion() {
        let (mut s, _d) = svc();
        let run = s.record_prompt(prompt("p1")).unwrap().run_id;
        let forged = EventPayload::MilestoneReached {
            milestone_id: "happy-path-demo".into(),
            trust: Trust::EvidenceVerified,
            evidence: vec![],
            claimant: Claimant::User,
        };
        assert!(matches!(
            append(&mut s, &run, "x", forged),
            Err(Error::BadRequest(_))
        ));
        let finish = EventPayload::RunFinished {
            outcome: RunOutcome::Completed,
            reason: None,
        };
        assert!(matches!(
            append(&mut s, &run, "y", finish),
            Err(Error::BadRequest(_))
        ));
    }

    #[test]
    fn git_evidence_verifies_and_completes_run() {
        let (mut s, d) = svc();
        let repo = d.path().join("repo");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let sha = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout)
            .unwrap()
            .trim()
            .to_string();

        let mut p = prompt("p1");
        p.project_root = Some(repo.to_string_lossy().into());
        // The commit must be made during the run (audit A3-1), so the run starts before it.
        p.occurred_at = Some(Utc::now() - Duration::hours(1));
        let run = s.record_prompt(p).unwrap().run_id;
        let reach = |id: &str| EventPayload::MilestoneReached {
            milestone_id: id.into(),
            trust: Trust::SelfDeclared,
            evidence: vec![Evidence::GitCommit { sha: sha.clone() }],
            claimant: Claimant::User,
        };
        append(&mut s, &run, "a", reach("scaffold-runs")).unwrap();
        assert_eq!(s.run(&run).unwrap().status, RunStatus::Running);
        let resp = append(&mut s, &run, "b", reach("happy-path-demo")).unwrap();
        assert_eq!(resp.derived.len(), 2, "verified + completed: {:?}", resp.derived);
        let view = s.run(&run).unwrap();
        assert_eq!(view.status, RunStatus::Completed);
        assert!(s.verify_chain(&run).unwrap().ok);
    }

    #[test]
    fn abandoned_run_is_replaced_by_next_prompt() {
        let (mut s, _d) = svc();
        let first = s.record_prompt(prompt("p1")).unwrap().run_id;
        let reset = EventPayload::RunFinished {
            outcome: RunOutcome::Abandoned,
            reason: None,
        };
        append(&mut s, &first, "reset", reset).unwrap();
        assert!(s.active_run("proj").unwrap().is_none());
        let second = s.record_prompt(prompt("p2")).unwrap();
        assert!(second.run_started);
        assert_ne!(second.run_id, first);
    }

    #[test]
    fn live_snapshot_compares_against_personal_best() {
        let (mut s, _d) = svc();
        let complete = |s: &mut Service, project: &str, n: &str| {
            let mut p = prompt(&format!("{n}-p"));
            p.project_id = project.into();
            let run = s.record_prompt(p).unwrap().run_id;
            for key in ["scaffold-runs", "happy-path-demo"] {
                // happy-path-demo needs verified evidence; inject it the way a verifier would.
                append(
                    s,
                    &run,
                    &format!("{n}-{key}"),
                    EventPayload::MilestoneReached {
                        milestone_id: key.into(),
                        trust: Trust::SelfDeclared,
                        evidence: vec![],
                        claimant: Claimant::User,
                    },
                )
                .unwrap();
                s.insert_derived(
                    &run,
                    &format!("{n}-{key}-v"),
                    Utc::now(),
                    EventPayload::MilestoneVerified {
                        milestone_id: key.into(),
                        trust: Trust::EvidenceVerified,
                        verifier: "test".into(),
                        evidence: vec![],
                    },
                )
                .unwrap();
                s.complete_if_done(&run).unwrap();
            }
            s.pb_cache.clear();
            run
        };
        let first = complete(&mut s, "a", "one");
        let live = s.live(Duration::days(1)).unwrap();
        assert_eq!(live.runs.len(), 1);
        assert_eq!(live.runs[0].status, RunStatus::Completed);
        assert!(live.runs[0].pb.is_none(), "a run is not its own PB");

        // The next run of the same project, after a reset closes the completed one, beside a
        // run of another project: its PB is the project's own completed run (A6-6).
        let reset = EventPayload::RunFinished {
            outcome: RunOutcome::Abandoned,
            reason: None,
        };
        append(&mut s, &first, "one-reset", reset).unwrap();
        let next = s
            .record_prompt(PromptRequest {
                project_id: "a".into(),
                ..prompt("a-p2")
            })
            .unwrap()
            .run_id;
        s.record_prompt(PromptRequest {
            project_id: "b".into(),
            ..prompt("b-p")
        })
        .unwrap();
        let live = s.live(Duration::days(1)).unwrap();
        assert_eq!(live.runs.len(), 2);
        let shown = live.runs.iter().find(|r| r.run_id == next).unwrap();
        assert_eq!(shown.status, RunStatus::Running);
        assert_eq!(shown.pb.as_ref().unwrap().run_id, first);
        assert!(shown.running.agent);
        assert!(live.cursor > 0);
        assert!(
            s.live(Duration::zero()).unwrap().runs.len() < 2,
            "window filters stale runs"
        );
    }

    #[test]
    fn late_verification_is_dated_when_checked() {
        let (mut s, _d) = svc();
        // The run starts before the late claim, so the claim is late but not backdated (A2-6).
        let mut p = prompt("p1");
        p.occurred_at = Some(Utc::now() - Duration::hours(2));
        let run = s.record_prompt(p).unwrap().run_id;
        let claim = |s: &mut Service, id: &str, at: DateTime<Utc>| {
            s.append(AppendRequest {
                event_id: id.into(),
                run_id: run.clone(),
                occurred_at: Some(at),
                payload: EventPayload::MilestoneReached {
                    milestone_id: "happy-path-demo".into(),
                    trust: Trust::SelfDeclared,
                    evidence: vec![Evidence::Url {
                        url: "https://demo.acme.dev".into(),
                    }],
                    claimant: Claimant::User,
                },
            })
            .unwrap()
            .job
            .expect("url evidence needs a check")
        };
        let passed = verify::Outcome {
            verifiers: vec!["url"],
            problems: vec![],
        };
        let failed = verify::Outcome {
            verifiers: vec![],
            problems: vec!["down".into()],
        };

        let late = claim(&mut s, "late", Utc::now() - Duration::hours(1));
        assert!(s.finish_verification(&late, &failed).unwrap().is_empty());
        assert!(
            s.run(&run).unwrap().milestone("happy-path-demo").unwrap().trust < Some(Trust::EvidenceVerified)
        );
        let ev = s.finish_verification(&late, &passed).unwrap();
        assert!(
            Utc::now() - ev[0].occurred_at < Duration::minutes(1),
            "dated when checked"
        );
    }

    #[test]
    fn verification_is_dated_at_the_claim() {
        let (mut s, _d) = svc();
        let run = s.record_prompt(prompt("p1")).unwrap().run_id;
        let at = Utc::now() - Duration::minutes(2);
        let job = s
            .append(AppendRequest {
                event_id: "c".into(),
                run_id: run.clone(),
                occurred_at: Some(at),
                payload: EventPayload::MilestoneReached {
                    milestone_id: "happy-path-demo".into(),
                    trust: Trust::SelfDeclared,
                    evidence: vec![Evidence::Url {
                        url: "https://demo.acme.dev".into(),
                    }],
                    claimant: Claimant::User,
                },
            })
            .unwrap()
            .job
            .unwrap();
        assert_eq!(job.key.as_deref(), Some("happy-path-demo"));
        let passed = verify::Outcome {
            verifiers: vec!["url"],
            problems: vec![],
        };
        let ev = s.finish_verification(&job, &passed).unwrap();
        assert_eq!(ev[0].occurred_at, at);
        assert_eq!(ev.len(), 1, "scaffold-runs still open, so no completion");
        assert!(
            s.finish_verification(&job, &passed).unwrap().is_empty(),
            "idempotent"
        );
    }

    #[test]
    fn database_rejects_rewriting_history() {
        let (mut s, _d) = svc();
        s.record_prompt(prompt("p1")).unwrap();
        let res: std::result::Result<(), Error> = s.store.transaction(|tx| {
            tx.raw().execute("UPDATE events SET body = '{}'", [])?;
            Ok(())
        });
        assert!(res.is_err());
        let res: std::result::Result<(), Error> = s.store.transaction(|tx| {
            tx.raw().execute("DELETE FROM events", [])?;
            Ok(())
        });
        assert!(res.is_err());
    }

    #[test]
    fn every_event_is_signed_by_the_device() {
        let (mut s, _d) = svc();
        let r = s.record_prompt(prompt("p1")).unwrap();
        let report = s.verify_chain(&r.run_id).unwrap();
        assert!(report.ok, "{:?}", report.error);
        assert_eq!(report.signed, report.events);
        assert_eq!(report.unsigned, 0);
        for e in s.events(&r.run_id).unwrap() {
            assert!(e.sig.is_some(), "seq {} is unsigned", e.seq);
        }
    }

    /// The owner of the machine owns SQLite: they can drop the append-only
    /// triggers and re-hash whatever they edit. The signature is what still
    /// catches it.
    #[test]
    fn rewriting_and_rehashing_an_event_fails_the_signature_check() {
        let (mut s, _d) = svc();
        let r = s.record_prompt(prompt("p1")).unwrap();

        let mut events = s.events(&r.run_id).unwrap();
        let last = events.last_mut().unwrap();
        last.recorded_at += Duration::seconds(1);
        last.hash = speedrun_core::hash::compute_hash(last);
        let (seq, body) = (last.seq as i64, serde_json::to_string(&last).unwrap());

        s.store
            .transaction(|tx| {
                tx.raw().execute_batch("DROP TRIGGER events_no_update")?;
                tx.raw().execute(
                    "UPDATE events SET body = ?1 WHERE run_id = ?2 AND seq = ?3",
                    rusqlite::params![body, r.run_id, seq],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .unwrap();

        let report = s.verify_chain(&r.run_id).unwrap();
        assert!(!report.ok);
        assert!(
            report.error.unwrap().contains("signature"),
            "a re-hashed edit must fail on the signature, not silently pass"
        );
    }

    // --- Audit regressions (docs/audit/v0.1.0.json, docs/V0.2.md).

    #[test]
    fn audit_a2_2_stale_job_does_not_verify_a_reclaimed_milestone() {
        let (mut s, _d) = svc();
        let run = s.record_prompt(prompt("p1")).unwrap().run_id;
        let claim = |evidence: Evidence| EventPayload::MilestoneReached {
            milestone_id: "happy-path-demo".into(),
            trust: Trust::SelfDeclared,
            evidence: vec![evidence],
            claimant: Claimant::User,
        };
        let request = |id: &str, payload: EventPayload| AppendRequest {
            event_id: id.into(),
            run_id: run.clone(),
            occurred_at: None,
            payload,
        };
        let stale = s
            .append(request(
                "c1",
                claim(Evidence::Url {
                    url: "https://demo.acme.dev".into(),
                }),
            ))
            .unwrap()
            .job
            .expect("url evidence needs a check");
        s.append(request(
            "i1",
            EventPayload::MilestoneInvalidated {
                milestone_id: "happy-path-demo".into(),
                reason: "wrong demo".into(),
            },
        ))
        .unwrap();
        s.append(request(
            "c2",
            claim(Evidence::Note {
                text: "done now".into(),
            }),
        ))
        .unwrap();

        let passed = verify::Outcome {
            verifiers: vec!["url"],
            problems: vec![],
        };
        assert!(s.finish_verification(&stale, &passed).unwrap().is_empty());
        let m = s.run(&run).unwrap();
        let m = m.milestone("happy-path-demo").unwrap();
        assert_ne!(m.state, MilestoneState::Verified);
        assert!(m.trust < Some(Trust::EvidenceVerified));
    }

    /// Complete a run of `project` that started `ago` before now, then reset it so the
    /// project's next prompt starts a new run. The completed result is kept.
    fn completed_run(s: &mut Service, project: &str, name: &str, ago: Duration) -> String {
        let mut p = prompt(&format!("{name}-p"));
        p.project_id = project.into();
        p.occurred_at = Some(Utc::now() - ago);
        let run = s.record_prompt(p).unwrap().run_id;
        for key in ["scaffold-runs", "happy-path-demo"] {
            let claim = EventPayload::MilestoneReached {
                milestone_id: key.into(),
                trust: Trust::SelfDeclared,
                evidence: vec![],
                claimant: Claimant::User,
            };
            append(s, &run, &format!("{name}-{key}"), claim).unwrap();
            // Inject the verification the way a verifier would.
            let verified = EventPayload::MilestoneVerified {
                milestone_id: key.into(),
                trust: Trust::EvidenceVerified,
                verifier: "test".into(),
                evidence: vec![],
            };
            s.insert_derived(&run, &format!("{name}-{key}-v"), Utc::now(), verified)
                .unwrap();
            s.complete_if_done(&run).unwrap();
        }
        assert_eq!(s.run(&run).unwrap().status, RunStatus::Completed);
        let reset = EventPayload::RunFinished {
            outcome: RunOutcome::Abandoned,
            reason: None,
        };
        append(s, &run, &format!("{name}-reset"), reset).unwrap();
        s.pb_cache.clear();
        run
    }

    /// The PB the live snapshot shows for `project`'s current run, with the project it came from.
    fn live_pb(s: &mut Service, project: &str) -> Option<(String, String)> {
        let live = s.live(Duration::days(1)).unwrap();
        let shown = live
            .runs
            .iter()
            .find(|r| r.project_id == project)
            .unwrap_or_else(|| panic!("{project} has a live run"));
        let pb = shown.pb.as_ref()?;
        let from = s.run(&pb.run_id).unwrap().project_id;
        Some((pb.run_id.clone(), from))
    }

    /// W7.a: the PB shown comes from the same project (V0.2 W7: PBs scoped to the project by default).
    #[test]
    fn audit_w7_a_pb_is_the_projects_own_run_not_a_faster_foreign_one() {
        let (mut s, _d) = svc();
        completed_run(&mut s, "other", "fast", Duration::minutes(1));
        let own = completed_run(&mut s, "mine", "slow", Duration::hours(2));
        s.record_prompt(PromptRequest {
            project_id: "mine".into(),
            ..prompt("mine-now")
        })
        .unwrap();
        assert_eq!(
            live_pb(&mut s, "mine").map(|(run, _)| run),
            Some(own),
            "the PB is this project's earlier run, not the faster run of another project"
        );
    }

    /// W7.a: the PB shown comes from the same project (V0.2 W7: PBs scoped to the project by default).
    #[test]
    fn audit_w7_a_project_without_a_completed_run_shows_no_foreign_pb() {
        let (mut s, _d) = svc();
        completed_run(&mut s, "other", "fast", Duration::minutes(1));
        s.record_prompt(PromptRequest {
            project_id: "fresh".into(),
            ..prompt("fresh-now")
        })
        .unwrap();
        if let Some((run, from)) = live_pb(&mut s, "fresh") {
            assert_eq!(from, "fresh", "PB {run} comes from another project");
        }
    }

    /// W7.a guard: a project's own earlier completed run is still shown as its PB.
    #[test]
    fn guard_w7_a_projects_own_completed_run_is_its_pb() {
        let (mut s, _d) = svc();
        let own = completed_run(&mut s, "mine", "first", Duration::hours(1));
        s.record_prompt(PromptRequest {
            project_id: "mine".into(),
            ..prompt("mine-now")
        })
        .unwrap();
        assert_eq!(live_pb(&mut s, "mine"), Some((own, "mine".to_string())));
    }

    fn rotations_in(s: &Service, run: &str) -> usize {
        s.events(run)
            .unwrap()
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::DeviceRotated { .. }))
            .count()
    }

    fn open_in(dir: &tempfile::TempDir) -> Service {
        let store = Store::open(&dir.path().join("db.sqlite"), &dir.path().join("device.key")).unwrap();
        Service::new(store, category::builtin().unwrap(), "test-source".into())
    }

    /// W9.c guard: a run that did not write between two rotations gets both
    /// certificates, in order, before its next event.
    #[test]
    fn guard_w9_c_two_rotations_between_writes_keep_the_run_verifiable() {
        let (mut s, _d) = svc();
        let run = s.record_prompt(prompt("p1")).unwrap().run_id;
        s.rotate_device().unwrap();
        s.rotate_device().unwrap();
        s.record_prompt(prompt("p2")).unwrap();
        let report = s.verify_chain(&run).unwrap();
        assert!(report.ok, "{:?}", report.error);
        assert_eq!(rotations_in(&s, &run), 2);
    }

    /// W9.c guard: a rotation is still carried to older chains after a restart.
    #[test]
    fn guard_w9_c_a_rotation_reaches_older_runs_after_a_restart() {
        let (mut s, d) = svc();
        let run = s.record_prompt(prompt("p1")).unwrap().run_id;
        s.rotate_device().unwrap();
        drop(s);
        let mut s = open_in(&d);
        s.record_prompt(prompt("p2")).unwrap();
        let report = s.verify_chain(&run).unwrap();
        assert!(report.ok, "{:?}", report.error);
        assert_eq!(rotations_in(&s, &run), 1);
    }

    /// W9.c guard: a rotation that stopped before committing its events is
    /// undone at the next start, and the completed rotation before it stands.
    #[test]
    fn guard_w9_c_an_interrupted_rotation_keeps_the_earlier_ones() {
        let (mut s, d) = svc();
        let run = s.record_prompt(prompt("p1")).unwrap().run_id;
        let first = s.rotate_device().unwrap().device_pubkey;
        drop(s);
        // The second rotation wrote its new key, then stopped before its commit.
        let pending = d.path().join("device.key.next");
        std::fs::write(&pending, "09".repeat(32)).unwrap();
        let mut s = open_in(&d);
        assert!(!pending.exists(), "an uncommitted rotation's key is removed");
        s.record_prompt(prompt("p2")).unwrap();
        let report = s.verify_chain(&run).unwrap();
        assert!(report.ok, "{:?}", report.error);
        assert_eq!(rotations_in(&s, &run), 1);
        let seed = std::fs::read_to_string(d.path().join("device.key")).unwrap();
        let key = speedrun_core::sign::parse_seed(&seed).unwrap();
        assert_eq!(speedrun_core::sign::pubkey_hex(&key), first);
    }

    /// W9.c guard: a rotation that committed its events but stopped before
    /// renaming its key file is finished at the next start.
    #[test]
    fn guard_w9_c_a_committed_rotation_is_finished_at_the_next_start() {
        let (s, d) = svc();
        drop(s);
        let next = speedrun_core::sign::parse_seed(&"09".repeat(32)).unwrap();
        let next_pubkey = speedrun_core::sign::pubkey_hex(&next);
        std::fs::write(d.path().join("device.key.next"), "09".repeat(32)).unwrap();
        let conn = rusqlite::Connection::open(d.path().join("db.sqlite")).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('signing_pubkey', ?1)",
            [&next_pubkey],
        )
        .unwrap();
        drop(conn);
        let s = open_in(&d);
        assert_eq!(s.store.device_pubkey(), next_pubkey);
        assert!(!d.path().join("device.key.next").exists());
        let seed = std::fs::read_to_string(d.path().join("device.key")).unwrap();
        assert_eq!(seed.trim(), "09".repeat(32));
    }
}
