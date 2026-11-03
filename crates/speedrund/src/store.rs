//! SQLite persistence. The `events` table is the source of truth and is
//! append-only, enforced by triggers. Everything else is derived.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use speedrun_core::{
    Event, EventPayload, SCHEMA_VERSION,
    api::RotateResponse,
    hash::compute_hash,
    sign::{SEED_LEN, fingerprint, generate, parse_seed, pubkey_hex, rotation_certificate, sign_hash},
};

use crate::disclosure::Disclosure;

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE events (
    event_id    TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    type        TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    hash        TEXT NOT NULL,
    body        TEXT NOT NULL,
    UNIQUE (run_id, seq)
);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;

-- Index of runs per project, written with each run's RunStarted event.
CREATE TABLE runs (
    run_id     TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    created    INTEGER NOT NULL
);
CREATE INDEX runs_by_project ON runs (project_id, created);

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#,
    r#"
ALTER TABLE runs ADD COLUMN category_id TEXT;
ALTER TABLE runs ADD COLUMN category_version INTEGER;
UPDATE runs SET
    category_id = (SELECT json_extract(body, '$.payload.category_id') FROM events e
                   WHERE e.run_id = runs.run_id AND e.seq = 1),
    category_version = (SELECT json_extract(body, '$.payload.category_version') FROM events e
                        WHERE e.run_id = runs.run_id AND e.seq = 1);
CREATE INDEX runs_by_category ON runs (category_id, category_version);
"#,
    r#"
-- Prompt text lives outside the append-only chain, keyed by its event id.
-- There is deliberately no delete trigger: a user may erase prompt text
-- without touching the events that commit to it.
CREATE TABLE prompts (
    event_id TEXT PRIMARY KEY,
    content  TEXT NOT NULL
);
"#,
    r#"
-- A run's project root is local state, never part of the published log.
-- New runs record it here in the same transaction as their RunStarted.
CREATE TABLE run_roots (
    run_id       TEXT PRIMARY KEY,
    project_root TEXT
);
"#,
    r#"
CREATE TABLE verification_jobs (
    cause_event_id TEXT PRIMARY KEY REFERENCES events(event_id),
    state TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    report TEXT
);
CREATE INDEX verification_jobs_pending ON verification_jobs(state, updated_at);
CREATE TABLE local_test_observations (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    head TEXT NOT NULL,
    body TEXT NOT NULL,
    succeeded INTEGER NOT NULL
);
"#,
];

pub struct Store {
    conn: Connection,
    device_id: String,
    key: SigningKey,
    /// Where `key` is kept, so a rotation can replace it.
    key_path: PathBuf,
    prompt_key: [u8; SEED_LEN],
    disclosure: Disclosure,
}

impl Store {
    pub fn record_local_test(
        &self,
        id: &str,
        run_id: &str,
        project_id: &str,
        head: &str,
        body: &str,
        succeeded: bool,
    ) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO local_test_observations (id, run_id, project_id, head, body, succeeded) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO UPDATE SET body = excluded.body, succeeded = excluded.succeeded WHERE run_id = excluded.run_id AND project_id = excluded.project_id AND head = excluded.head", params![id, run_id, project_id, head, body, succeeded])?;
        Ok(())
    }

    pub fn local_test_head(
        &self,
        id: &str,
        run_id: &str,
        project_id: &str,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT head FROM local_test_observations WHERE id = ?1 AND run_id = ?2 AND project_id = ?3 AND succeeded = 1",
                params![id, run_id, project_id],
                |r| r.get(0),
            )
            .optional()
    }
    pub fn local_test_attempts(&self, run_id: &str) -> rusqlite::Result<Vec<String>> {
        let mut query = self.conn.prepare(
            "SELECT body FROM local_test_observations WHERE run_id = ?1 ORDER BY rowid DESC LIMIT 100",
        )?;
        query.query_map([run_id], |row| row.get(0))?.collect()
    }

    pub fn recover_verification_jobs(&self) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE verification_jobs SET state = 'pending' WHERE state = 'running'",
            [],
        )?;
        self.conn.execute("UPDATE local_test_observations SET body = json_set(body, '$.status.kind', 'interrupted'), succeeded = 0 WHERE json_extract(body, '$.status.kind') = 'running'", [])?;
        Ok(())
    }

    pub fn next_verification_job(&self) -> rusqlite::Result<Option<String>> {
        self.conn.execute(
            "UPDATE verification_jobs SET state = 'pending' WHERE state = 'running' AND updated_at < ?1",
            [(Utc::now() - chrono::Duration::minutes(10)).to_rfc3339()],
        )?;
        self.conn.query_row("SELECT cause_event_id FROM verification_jobs WHERE state = 'pending' ORDER BY updated_at, cause_event_id LIMIT 1", [], |r| r.get(0)).optional()
    }

    pub fn start_verification_job(&self, id: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("UPDATE verification_jobs SET state = 'running', attempts = attempts + 1, updated_at = ?2 WHERE cause_event_id = ?1 AND state = 'pending'", params![id, Utc::now().to_rfc3339()])? == 1)
    }

    pub fn retry_verification_job(&self, id: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE verification_jobs SET state = 'pending', updated_at = ?2 WHERE cause_event_id = ?1 AND state = 'running'", params![id, Utc::now().to_rfc3339()])?;
        Ok(())
    }

    pub fn finish_verification_job(&self, id: &str, state: &str, report: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE verification_jobs SET state = ?2, report = ?3, updated_at = ?4 WHERE cause_event_id = ?1",
            params![id, state, report, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn verification_job_status(
        &self,
        id: &str,
    ) -> rusqlite::Result<Option<(String, i64, Option<String>)>> {
        self.conn
            .query_row(
                "SELECT state, attempts, report FROM verification_jobs WHERE cause_event_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }

    /// Open the database and the device key at `key_path`, which signs every
    /// event appended from now on. The device id is the fingerprint of the
    /// first key this database saw; it does not change when the key rotates.
    pub fn open(path: &Path, key_path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // The schema gate reads only; it must run before any pragma or
        // migration can change a database written by a newer build.
        let schema = Schema::THIS_BUILD;
        schema.refuse_newer(&conn, path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema.apply(&mut conn)?;
        let key = load_device_key(&conn, key_path)?;
        let fingerprint = fingerprint(&pubkey_hex(&key)).map_err(anyhow::Error::msg)?;
        let device_id = match conn
            .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| r.get(0))
            .optional()?
        {
            Some(id) => id,
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('device_id', ?1)",
                    [&fingerprint],
                )?;
                fingerprint
            }
        };
        let prompt_key_path = path.with_file_name(crate::keys::PROMPT_KEY_FILE);
        let prompt_key = crate::keys::load_or_create_secret(&prompt_key_path)?;
        Ok(Self {
            conn,
            device_id,
            key,
            key_path: key_path.to_path_buf(),
            prompt_key,
            disclosure: Disclosure::beside(path),
        })
    }

    /// Copy the whole database to `target`, a file that must not exist yet,
    /// and return how many events the copy holds. `VACUUM INTO` reads one
    /// consistent snapshot, so writes in the write-ahead log are included and
    /// none arrives half-way. The copy is created 0600 first, because it
    /// holds prompt text; a failed copy leaves no file behind.
    pub fn backup(&self, target: &Path) -> std::io::Result<u64> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(target)?;
        let copied = self.copy_into(target);
        if copied.is_err()
            && let Err(e) = std::fs::remove_file(target)
        {
            tracing::warn!("could not remove the failed backup {}: {e}", target.display());
        }
        copied.map_err(std::io::Error::other)
    }

    fn copy_into(&self, target: &Path) -> rusqlite::Result<u64> {
        let name = target.to_string_lossy();
        self.conn.execute("VACUUM INTO ?1", [name.as_ref()])?;
        let copy = Connection::open_with_flags(target, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let events: i64 = copy.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
        // A count is never negative, so this is exact.
        Ok(events.unsigned_abs())
    }

    /// Run `f` in an IMMEDIATE transaction; commit only if it returns Ok.
    pub fn transaction<T, E: From<rusqlite::Error>>(
        &mut self,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T, E>,
    ) -> Result<T, E> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut w = Tx {
            tx,
            device_id: &self.device_id,
            key: &self.key,
            prompt_key: &self.prompt_key,
            disclosure: &self.disclosure,
        };
        let out = f(&mut w)?;
        w.tx.commit()?;
        Ok(out)
    }

    pub fn reader(&self) -> Reader<'_> {
        Reader(&self.conn)
    }

    /// Hex public key this device signs with now.
    pub fn device_pubkey(&self) -> String {
        pubkey_hex(&self.key)
    }

    /// Sign a publication digest with the current device key: the bundle's
    /// `signature`, checkable against [`Store::device_pubkey`].
    pub fn sign_digest(&self, digest: &str) -> String {
        sign_hash(&self.key, digest)
    }

    /// Read the disclosure key a run started with, for a tier 2 export.
    pub fn disclosure_key(&self, run_id: &str) -> Result<[u8; SEED_LEN]> {
        self.disclosure.load(run_id)
    }

    /// Replace this device's signing key. Every chain this device keeps that
    /// names the old key gets a `DeviceRotated` now, signed by the new key and
    /// carrying the old key's certificate, so each run verifies from public
    /// keys alone and no later write has to catch up. The old key is not kept.
    ///
    /// The new key is written beside the current one first, the events are
    /// committed in one transaction that also records the new key, and only
    /// then is the file renamed into place; `open` finishes or undoes a
    /// rotation that stopped in between.
    pub fn rotate_key(&mut self) -> Result<RotateResponse> {
        let previous_pubkey = pubkey_hex(&self.key);
        let new = generate();
        let device_pubkey = pubkey_hex(&new);
        let certificate = rotation_certificate(&self.key, &device_pubkey);
        let pending = pending_key_path(&self.key_path);
        crate::keys::replace(&pending, &new)?;

        // The transaction signs with the new key; it ends before the key is kept.
        {
            let tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let mut w = Tx {
                tx,
                device_id: &self.device_id,
                key: &new,
                prompt_key: &self.prompt_key,
                disclosure: &self.disclosure,
            };
            let runs: Vec<String> = {
                let mut stmt = w.tx.prepare("SELECT run_id FROM runs ORDER BY created")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for run_id in &runs {
                if w.reader().chain_key(run_id, self.device_id.as_str())?.as_deref()
                    == Some(previous_pubkey.as_str())
                {
                    w.insert(
                        run_id,
                        &format!("{run_id}:rotate:{device_pubkey}"),
                        Utc::now(),
                        EventPayload::DeviceRotated {
                            new_pubkey: device_pubkey.clone(),
                            certificate: certificate.clone(),
                        },
                    )?;
                }
            }
            w.tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('signing_pubkey', ?1)",
                [&device_pubkey],
            )?;
            w.tx.commit()?;
        }

        fs::rename(&pending, &self.key_path)
            .with_context(|| format!("replacing {}", self.key_path.display()))?;
        self.key = new;
        Ok(RotateResponse {
            device_pubkey,
            previous_pubkey,
        })
    }
}

/// Where a rotation keeps the new key until its events are committed.
fn pending_key_path(key_path: &Path) -> PathBuf {
    let mut path = key_path.as_os_str().to_os_string();
    path.push(".next");
    PathBuf::from(path)
}

/// The key this device signs with. A rotation that stopped after committing
/// its events is finished (its new key, which the chains already name, is
/// moved into place); one that stopped before is undone.
fn load_device_key(conn: &Connection, key_path: &Path) -> Result<SigningKey> {
    let pending = pending_key_path(key_path);
    match fs::read_to_string(&pending) {
        Ok(seed) => {
            let next = parse_seed(&seed).map_err(|e| anyhow!("{e}; in {}", pending.display()))?;
            let committed: Option<String> = conn
                .query_row("SELECT value FROM meta WHERE key = 'signing_pubkey'", [], |r| {
                    r.get(0)
                })
                .optional()?;
            if committed.as_deref() == Some(pubkey_hex(&next).as_str()) {
                fs::rename(&pending, key_path)
                    .with_context(|| format!("replacing {}", key_path.display()))?;
            } else {
                fs::remove_file(&pending).with_context(|| format!("removing {}", pending.display()))?;
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", pending.display())),
    }
    crate::keys::load_or_create(key_path)
}

/// The schema contract between this build and a database file.
///
/// It is the only place that interprets `user_version`: a database above
/// `supported` belongs to a newer build and is refused, never migrated. The
/// startup path depends on this stable bound; the store never reaches up to
/// HTTP, logging or the binary's exit handling to report it.
struct Schema {
    supported: i64,
}

impl Schema {
    /// The migration list this build carries defines what it can open.
    const THIS_BUILD: Self = Self {
        supported: MIGRATIONS.len() as i64,
    };

    /// Read-only: refuse a database written by a newer build. Callers run
    /// this before any pragma that would change the file, so a newer
    /// database is left exactly as it was found.
    fn refuse_newer(&self, conn: &Connection, path: &Path) -> Result<()> {
        let found: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found > self.supported {
            return Err(anyhow!(
                "database {} is at schema version {found}, newer than this build supports ({})",
                path.display(),
                self.supported
            ));
        }
        Ok(())
    }

    /// Apply the migrations this database is missing. `refuse_newer` has
    /// already bounded the stored version by `supported`.
    fn apply(&self, conn: &mut Connection) -> Result<()> {
        let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let pending = MIGRATIONS
            .iter()
            .enumerate()
            .take(self.supported as usize)
            .skip(current as usize);
        for (i, sql) in pending {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("migration {}", i + 1))?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(())
    }
}

/// A stored run log: the rows that decoded into events, plus the rows that
/// did not. Decoding policy lives here, so callers never see raw bodies.
pub struct RunLog {
    pub events: Vec<Event>,
    pub unreadable: Vec<UnreadableRow>,
}

/// One stored row whose body is not an `Event`. `event_id` comes from the
/// column, so it is known even when the body is truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableRow {
    pub run_id: String,
    pub event_id: String,
    pub error: String,
}

/// Read-only queries, usable on the connection or inside a transaction.
pub struct Reader<'c>(&'c Connection);

impl Reader<'_> {
    pub fn event(&self, event_id: &str) -> rusqlite::Result<Option<Event>> {
        self.0
            .query_row("SELECT body FROM events WHERE event_id = ?1", [event_id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(|b| parse(&b))
            .transpose()
    }

    /// The redacted prompt text stored for `event_id`, if it has not been
    /// deleted. The text lives outside the chain, so a missing row is normal.
    pub fn prompt_text(&self, event_id: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT content FROM prompts WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .optional()
    }

    /// The project root a run started with, if it has one.
    ///
    /// New runs keep it in `run_roots`, outside the published log. A run
    /// written before that table existed falls back to the root its
    /// `RunStarted` stored; a row here, even with a NULL root, wins so a new
    /// run never consults its log.
    pub fn run_root(&self, run_id: &str) -> rusqlite::Result<Option<String>> {
        let stored: Option<Option<String>> = self
            .0
            .query_row(
                "SELECT project_root FROM run_roots WHERE run_id = ?1",
                [run_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        match stored {
            Some(root) => Ok(root),
            None => self.legacy_run_root(run_id),
        }
    }

    fn legacy_run_root(&self, run_id: &str) -> rusqlite::Result<Option<String>> {
        let body: Option<String> = self
            .0
            .query_row(
                "SELECT body FROM events WHERE run_id = ?1 AND type = 'run_started'
                 ORDER BY seq ASC LIMIT 1",
                [run_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(body) = body else {
            return Ok(None);
        };
        let body: serde_json::Value = parse_body(&body)?;
        Ok(body
            .get("payload")
            .and_then(|payload| payload.get("project_root"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string))
    }

    pub fn run_events(&self, run_id: &str) -> rusqlite::Result<Vec<Event>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT body FROM events WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([run_id], |r| r.get::<_, String>(0))?;
        rows.map(|b| parse(&b?)).collect()
    }

    /// The run's event bodies exactly as stored, ordered by seq. Unlike
    /// [`Reader::run_events`] this does not require a body to parse as this
    /// build's [`Event`], so a chain check can see fields it does not know.
    pub fn run_bodies(&self, run_id: &str) -> rusqlite::Result<Vec<serde_json::Value>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT body FROM events WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([run_id], |r| r.get::<_, String>(0))?;
        rows.map(|b| parse_body(&b?)).collect()
    }

    /// A run's events paired with the body each was stored as, ordered by
    /// `device_id` then `seq`: the order a publication publishes them in. One
    /// query, because a bundle needs the typed event to fold and the exact
    /// stored body to publish verbatim.
    pub fn run_rows(&self, run_id: &str) -> rusqlite::Result<Vec<(Event, serde_json::Value)>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT body FROM events WHERE run_id = ?1
             ORDER BY json_extract(body, '$.device_id'), json_extract(body, '$.seq')",
        )?;
        let rows = stmt.query_map([run_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let body = row?;
            out.push((parse(&body)?, parse_body(&body)?));
        }
        Ok(out)
    }

    /// When the run's first and newest events (by `seq`) say they happened.
    /// `None` when the run has no events. A write is bounded against both: it
    /// may not be dated before the run started, nor (unless it is a spool
    /// replay) more than the skew allowance behind the newest event.
    pub fn run_bounds(&self, run_id: &str) -> rusqlite::Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
        let first: Option<String> = self
            .0
            .query_row(
                "SELECT body FROM events WHERE run_id = ?1 ORDER BY seq ASC LIMIT 1",
                [run_id],
                |r| r.get(0),
            )
            .optional()?;
        match first {
            Some(first) => {
                let last: String = self.0.query_row(
                    "SELECT body FROM events WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
                    [run_id],
                    |r| r.get(0),
                )?;
                Ok(Some((parse(&first)?.occurred_at, parse(&last)?.occurred_at)))
            }
            None => Ok(None),
        }
    }

    /// Read a run's log without failing on a row that does not decode: the
    /// rows that did are returned as events, and the rest are named here so
    /// the caller can report them instead of dropping them silently.
    /// The key `device_id`'s chain in `run_id` last named: the newest of its
    /// RunStarted, DeviceJoined and DeviceRotated events. Only those rows are
    /// read, so the cost does not grow with the run.
    pub fn chain_key(&self, run_id: &str, device_id: &str) -> rusqlite::Result<Option<String>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT body FROM events WHERE run_id = ?1
               AND type IN ('run_started', 'device_joined', 'device_rotated')
             ORDER BY seq DESC",
        )?;
        let mut rows = stmt.query([run_id])?;
        while let Some(row) = rows.next()? {
            let body: String = row.get(0)?;
            let event: Event = serde_json::from_str(&body).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
            })?;
            if event.device_id != device_id {
                continue;
            }
            match event.payload {
                EventPayload::RunStarted {
                    device_pubkey: Some(key),
                    ..
                }
                | EventPayload::DeviceJoined { device_pubkey: key }
                | EventPayload::DeviceRotated { new_pubkey: key, .. } => return Ok(Some(key)),
                _ => {}
            }
        }
        Ok(None)
    }

    pub fn run_log(&self, run_id: &str) -> rusqlite::Result<RunLog> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT event_id, body FROM events WHERE run_id = ?1 ORDER BY seq")?;
        let mut rows = stmt.query([run_id])?;
        let mut log = RunLog {
            events: Vec::new(),
            unreadable: Vec::new(),
        };
        while let Some(row) = rows.next()? {
            let event_id: String = row.get(0)?;
            let body: String = row.get(1)?;
            match serde_json::from_str::<Event>(&body) {
                Ok(event) => log.events.push(event),
                Err(e) => log.unreadable.push(UnreadableRow {
                    run_id: run_id.to_string(),
                    event_id,
                    error: e.to_string(),
                }),
            }
        }
        Ok(log)
    }

    pub fn latest_run(&self, project_id: &str) -> rusqlite::Result<Option<String>> {
        self.0
            .query_row(
                "SELECT run_id FROM runs WHERE project_id = ?1 ORDER BY created DESC LIMIT 1",
                [project_id],
                |r| r.get(0),
            )
            .optional()
    }
}

impl Reader<'_> {
    /// Position of the newest event across all runs. The table is
    /// append-only, so rowid only grows.
    pub fn cursor(&self) -> rusqlite::Result<u64> {
        self.0
            .query_row("SELECT COALESCE(MAX(rowid), 0) FROM events", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n as u64)
    }

    /// All runs for a project, newest first.
    pub fn project_runs(&self, project_id: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT run_id FROM runs WHERE project_id = ?1 ORDER BY created DESC, rowid DESC",
        )?;
        let rows = stmt.query_map([project_id], |r| r.get(0))?;
        rows.collect()
    }

    /// The newest run of every project.
    pub fn latest_runs(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT run_id FROM runs r
             WHERE created = (SELECT MAX(created) FROM runs WHERE project_id = r.project_id)",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// Run ids of `project_id` in `category_id`@`version`. The PB a live run is
    /// compared against is drawn from this set, so another project's run can
    /// never be offered as its personal best.
    pub fn runs_in_category_for_project(
        &self,
        project_id: &str,
        category_id: &str,
        version: u32,
    ) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.0.prepare_cached(
            "SELECT run_id FROM runs
             WHERE project_id = ?1 AND category_id = ?2 AND category_version = ?3",
        )?;
        let rows = stmt.query_map(params![project_id, category_id, version], |r| r.get(0))?;
        rows.collect()
    }
}

fn parse(body: &str) -> rusqlite::Result<Event> {
    serde_json::from_str(body)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn parse_body(body: &str) -> rusqlite::Result<serde_json::Value> {
    serde_json::from_str(body)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

pub struct Tx<'c> {
    tx: Transaction<'c>,
    device_id: &'c str,
    key: &'c SigningKey,
    prompt_key: &'c [u8; SEED_LEN],
    disclosure: &'c Disclosure,
}

impl Tx<'_> {
    pub fn enqueue_verification(&self, event_id: &str) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT OR IGNORE INTO verification_jobs (cause_event_id, updated_at) VALUES (?1, ?2)",
            params![event_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
    pub fn reader(&self) -> Reader<'_> {
        Reader(&self.tx)
    }

    /// Hex public key of the device writing this transaction.
    pub fn device_pubkey(&self) -> String {
        pubkey_hex(self.key)
    }

    /// Create this run's disclosure key and return the commitment to record
    /// in `RunStarted`. The key itself never enters the log.
    pub fn disclose(&self, run_id: &str) -> anyhow::Result<String> {
        self.disclosure.create(run_id)
    }

    /// Keep a run's project root beside the log. Every new run records a
    /// row here, even when it has no root, so the log is never consulted for
    /// a root this table is responsible for.
    pub fn record_run_root(&self, run_id: &str, project_root: Option<&str>) -> rusqlite::Result<()> {
        self.tx.execute(
            "INSERT INTO run_roots (run_id, project_root) VALUES (?1, ?2)",
            params![run_id, project_root],
        )?;
        Ok(())
    }

    /// Append to the end of a run's chain. The caller validates first.
    pub fn insert(
        &mut self,
        run_id: &str,
        event_id: &str,
        occurred_at: DateTime<Utc>,
        payload: EventPayload,
    ) -> rusqlite::Result<Event> {
        let last: Option<(i64, String)> = self
            .tx
            .query_row(
                "SELECT seq, hash FROM events WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
                [run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut event = Event {
            event_id: event_id.to_string(),
            schema_version: SCHEMA_VERSION,
            run_id: run_id.to_string(),
            seq: last.as_ref().map_or(1, |(s, _)| *s as u64 + 1),
            device_id: self.device_id.to_string(),
            occurred_at,
            recorded_at: Utc::now(),
            payload,
            prev_hash: last.map(|(_, h)| h),
            hash: String::new(),
            sig: None,
        };
        event.hash = compute_hash(&event);
        event.sig = Some(sign_hash(self.key, &event.hash));
        let body = serde_json::to_string(&event).expect("Event always serializes");
        self.tx.execute(
            "INSERT INTO events (event_id, run_id, seq, type, occurred_at, recorded_at, hash, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event.event_id,
                event.run_id,
                event.seq as i64,
                event.payload.kind(),
                event.occurred_at.to_rfc3339(),
                event.recorded_at.to_rfc3339(),
                event.hash,
                body
            ],
        )?;
        if let EventPayload::RunStarted {
            project_id,
            category_id,
            category_version,
            ..
        } = &event.payload
        {
            self.tx.execute(
                "INSERT INTO runs (run_id, project_id, started_at, created, category_id, category_version)
                 VALUES (?1, ?2, ?3, (SELECT COALESCE(MAX(created), 0) + 1 FROM runs), ?4, ?5)",
                params![
                    event.run_id,
                    project_id,
                    event.occurred_at.to_rfc3339(),
                    category_id,
                    category_version
                ],
            )?;
        }
        Ok(event)
    }

    /// Append a prompt event and keep its redacted text in `prompts`, outside
    /// the chain. The digest is filled in here from that text under the
    /// daemon's prompt key, so no caller handles the key or can store text
    /// without its commitment.
    pub fn insert_prompt(
        &mut self,
        run_id: &str,
        event_id: &str,
        occurred_at: DateTime<Utc>,
        mut payload: EventPayload,
        text: &str,
    ) -> rusqlite::Result<Event> {
        if let EventPayload::PromptSubmitted { content_digest, .. } = &mut payload {
            *content_digest = speedrun_core::digest::text_digest(self.prompt_key, text);
        }
        let event = self.insert(run_id, event_id, occurred_at, payload)?;
        self.tx.execute(
            "INSERT INTO prompts (event_id, content) VALUES (?1, ?2)",
            params![event_id, text],
        )?;
        Ok(event)
    }

    #[cfg(test)]
    pub fn raw(&self) -> &Connection {
        &self.tx
    }
}
