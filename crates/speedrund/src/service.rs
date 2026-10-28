//! Business rules on top of the store: run lifecycle, validation,
//! verification and completion.

use chrono::{DateTime, Duration, Utc};
use speedrun_core::{
    Category, Event, EventPayload, MilestoneOrigin, RunOutcome, RunView, Trust,
    api::{AppendRequest, AppendResponse, ChainReport, ProjectAppendRequest, PromptRequest, PromptResponse},
    category,
    hash::verify_chain,
    redact::redact,
};

use crate::store::{Store, Tx};
use crate::verify;

/// Client clocks may run ahead by this much before being clamped.
const MAX_CLOCK_SKEW: Duration = Duration::minutes(5);
const MAX_ID_LEN: usize = 128;

#[derive(Debug)]
pub enum Error {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    Internal(anyhow::Error),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Service {
    store: Store,
    categories: Vec<Category>,
}

fn clamp_time(t: Option<DateTime<Utc>>) -> DateTime<Utc> {
    let now = Utc::now();
    t.map_or(now, |t| t.min(now + MAX_CLOCK_SKEW))
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
    RunView::fold(&events, Utc::now()).ok_or_else(|| Error::NotFound(format!("run {run_id}")))
}

impl Service {
    pub fn new(store: Store, categories: Vec<Category>) -> Self {
        Self { store, categories }
    }

    pub fn categories(&self) -> &[Category] {
        &self.categories
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
            let at = clamp_time(req.occurred_at);
            let current = match tx.reader().latest_run(&req.project_id)? {
                Some(run_id) => Some(fold(tx, &run_id)?),
                None => None,
            };
            let (run_id, run_started) = match current {
                Some(run) if !run.closed => (run.run_id, false),
                _ => {
                    let cat = category::find(categories, &req.category_id, None)
                        .ok_or_else(|| Error::BadRequest(format!("unknown category {}", req.category_id)))?;
                    (start_run(tx, &req, cat, at)?, true)
                }
            };
            let (content, redacted) = redact(&req.content);
            let event = tx.insert(
                &run_id,
                &req.event_id,
                at,
                EventPayload::PromptSubmitted {
                    session_id: req.session_id.clone(),
                    host: req.host.clone(),
                    model: req.model.clone(),
                    content,
                    redacted,
                },
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

    /// Append a client event to an existing run, then derive any
    /// verification or completion that follows from it.
    pub fn append(&mut self, req: AppendRequest) -> Result<AppendResponse> {
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
            if let Some(existing) = tx.reader().event(&req.event_id)? {
                if existing.run_id != req.run_id || existing.payload != req.payload {
                    return Err(Error::Conflict(format!(
                        "event_id {} already used for a different event",
                        req.event_id
                    )));
                }
                return Ok((existing, false));
            }
            let run = fold(tx, &req.run_id)?;
            run.check(&req.payload).map_err(Error::Conflict)?;
            let e = tx.insert(
                &req.run_id,
                &req.event_id,
                clamp_time(req.occurred_at),
                req.payload.clone(),
            )?;
            Ok((e, true))
        })?;

        // Derived steps run even on retries; their event ids make them idempotent.
        let derived = self.derive(&event)?;
        Ok(AppendResponse {
            event,
            inserted,
            derived,
        })
    }

    /// Append to the project's active run.
    pub fn append_to_project(
        &mut self,
        project_id: &str,
        req: ProjectAppendRequest,
    ) -> Result<AppendResponse> {
        // A retry must land in the run the original went to, even if a newer run exists.
        let run_id = match self.store.reader().event(&req.event_id)? {
            Some(existing) => existing.run_id,
            None => self.active_run(project_id)?.map(|r| r.run_id).ok_or_else(|| {
                Error::NotFound(format!(
                    "no active run for project {project_id}; send a prompt first"
                ))
            })?,
        };
        self.append(AppendRequest {
            event_id: req.event_id,
            run_id,
            occurred_at: req.occurred_at,
            payload: req.payload,
        })
    }

    fn derive(&mut self, cause: &Event) -> Result<Vec<Event>> {
        let mut out = Vec::new();
        if let EventPayload::MilestoneReached {
            milestone_id,
            evidence,
            ..
        } = &cause.payload
        {
            let root = {
                let events = self.store.reader().run_events(&cause.run_id)?;
                RunView::fold(&events, Utc::now()).and_then(|r| r.project_root)
            };
            if let Some(verifiers) = verify::verify(root.as_deref(), evidence) {
                let payload = EventPayload::MilestoneVerified {
                    milestone_id: milestone_id.clone(),
                    trust: Trust::EvidenceVerified,
                    verifier: verifiers.join("+"),
                    evidence: evidence.clone(),
                };
                out.extend(self.insert_derived(
                    &cause.run_id,
                    &format!("{}:verify", cause.event_id),
                    cause.occurred_at,
                    payload,
                )?);
            }
        }
        out.extend(self.complete_if_done(&cause.run_id)?);
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
            if fold(tx, run_id)?.check(&payload).is_err() {
                return Ok(None);
            }
            Ok(Some(tx.insert(run_id, event_id, at, payload)?))
        })
    }

    pub fn active_run(&self, project_id: &str) -> Result<Option<RunView>> {
        let r = self.store.reader();
        let Some(run_id) = r.latest_run(project_id)? else {
            return Ok(None);
        };
        Ok(RunView::fold(&r.run_events(&run_id)?, Utc::now()).filter(|v| !v.closed))
    }

    pub fn run(&self, run_id: &str) -> Result<RunView> {
        RunView::fold(&self.store.reader().run_events(run_id)?, Utc::now())
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
        let result = verify_chain(&events);
        Ok(ChainReport {
            run_id: run_id.to_string(),
            events: events.len() as u64,
            ok: result.is_ok(),
            error: result.err().map(|e| e.to_string()),
            head_hash: events.last().map(|e| e.hash.clone()),
        })
    }
}

fn start_run(tx: &mut Tx<'_>, req: &PromptRequest, cat: &Category, at: DateTime<Utc>) -> Result<String> {
    let run_id = speedrun_core::new_id();
    let base = &req.event_id;
    tx.insert(
        &run_id,
        &format!("{base}:run"),
        at,
        EventPayload::RunStarted {
            project_id: req.project_id.clone(),
            project_root: req.project_root.clone(),
            category_id: cat.id.clone(),
            category_version: cat.version,
        },
    )?;
    for m in &cat.milestones {
        tx.insert(
            &run_id,
            &format!("{base}:m:{}", m.key),
            at,
            EventPayload::MilestoneProposed {
                milestone_id: m.key.clone(),
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
                milestone_id: m.key.clone(),
            },
        )?;
    }
    Ok(run_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use speedrun_core::{Evidence, MilestoneState, RunStatus};

    fn svc() -> (Service, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        (Service::new(store, category::builtin()), dir)
    }

    fn prompt(id: &str) -> PromptRequest {
        PromptRequest {
            event_id: id.into(),
            project_id: "proj".into(),
            project_root: None,
            category_id: "any-percent".into(),
            session_id: "s1".into(),
            host: "test".into(),
            model: None,
            content: "build it. key: sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            occurred_at: None,
            starts_turn: true,
        }
    }

    fn append(s: &mut Service, run: &str, id: &str, payload: EventPayload) -> Result<AppendResponse> {
        s.append(AppendRequest {
            event_id: id.into(),
            run_id: run.into(),
            occurred_at: None,
            payload,
        })
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
        let EventPayload::PromptSubmitted {
            content, redacted, ..
        } = &r.event.payload
        else {
            panic!()
        };
        assert!(*redacted && !content.contains("sk-ant"));

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
        let run = s.record_prompt(p).unwrap().run_id;
        let reach = |id: &str| EventPayload::MilestoneReached {
            milestone_id: id.into(),
            trust: Trust::SelfDeclared,
            evidence: vec![Evidence::GitCommit { sha: sha.clone() }],
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
}
