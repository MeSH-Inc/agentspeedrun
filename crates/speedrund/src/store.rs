//! SQLite persistence. The `events` table is the source of truth and is
//! append-only, enforced by triggers. Everything else is derived.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use speedrun_core::{
    Event, EventPayload, SCHEMA_VERSION,
    api::RotateResponse,
    hash::compute_hash,
    sign::{SEED_LEN, fingerprint, generate, pubkey_hex, rotation_certificate, sign_hash},
};

use crate::disclosure::Disclosure;
use crate::keys::Rotation;

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

-- Index of runs per project; rebuilt from RunStarted events if lost.
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
];

pub struct Store {
    conn: Connection,
    device_id: String,
    key: SigningKey,
    /// The certified step from the key a run's chain last used to the current
    /// one, when this device has rotated since that chain last wrote.
    rotation: Option<Rotation>,
    /// The device key is a sibling of the database, like the disclosure keys.
    key_path: PathBuf,
    prompt_key: [u8; SEED_LEN],
    disclosure: Disclosure,
}

impl Store {
    /// Open the database. `key` signs every event appended from now on; the
    /// device id is its fingerprint, unless an older database already names a
    /// device (v1 archives keep the id they were written with).
    pub fn open(path: &Path, key: SigningKey) -> Result<Self> {
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
        let current_pubkey = pubkey_hex(&key);
        let fingerprint = fingerprint(&current_pubkey).map_err(anyhow::Error::msg)?;
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
        // A rotation is only in effect when the key file was replaced too;
        // otherwise the daemon that stopped between the two writes still
        // signs with the key on disk.
        let rotation = match conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'device_rotation'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            Some(json) => {
                let step: Rotation = serde_json::from_str(&json).map_err(anyhow::Error::msg)?;
                (step.new_pubkey == current_pubkey).then_some(step)
            }
            None => None,
        };
        let prompt_key_path = path.with_file_name(crate::keys::PROMPT_KEY_FILE);
        let prompt_key = crate::keys::load_or_create_secret(&prompt_key_path)?;
        Ok(Self {
            conn,
            device_id,
            key,
            rotation,
            key_path: path.with_file_name(crate::keys::DEVICE_KEY_FILE),
            prompt_key,
            disclosure: Disclosure::beside(path),
        })
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
            rotation: self.rotation.as_ref(),
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

    /// Read the disclosure key a run started with, for a tier 2 export.
    pub fn disclosure_key(&self, run_id: &str) -> Result<[u8; SEED_LEN]> {
        self.disclosure.load(run_id)
    }

    /// Replace this device's signing key. The certificate from the old key to
    /// the new one is kept so the next event written to each older chain can
    /// carry it; the old key itself is never stored again.
    pub fn rotate_key(&mut self) -> Result<RotateResponse> {
        let previous_pubkey = pubkey_hex(&self.key);
        let new = generate();
        let device_pubkey = pubkey_hex(&new);
        let rotation = Rotation {
            old_pubkey: previous_pubkey.clone(),
            new_pubkey: device_pubkey.clone(),
            certificate: rotation_certificate(&self.key, &device_pubkey),
        };
        let json = serde_json::to_string(&rotation).context("serializing the device rotation")?;
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('device_rotation', ?1)",
            [&json],
        )?;
        crate::keys::replace(&self.key_path, &new)?;
        self.key = new;
        self.rotation = Some(rotation);
        Ok(RotateResponse {
            device_pubkey,
            previous_pubkey,
        })
    }
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
    rotation: Option<&'c Rotation>,
    prompt_key: &'c [u8; SEED_LEN],
    disclosure: &'c Disclosure,
}

impl Tx<'_> {
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

    /// Append to the end of a run's chain. The caller validates first.
    pub fn insert(
        &mut self,
        run_id: &str,
        event_id: &str,
        occurred_at: DateTime<Utc>,
        payload: EventPayload,
    ) -> rusqlite::Result<Event> {
        // A write to a chain that still names the key before this device's
        // latest rotation carries that rotation first, so every earlier
        // signature can be checked from public keys alone.
        let mut pending: Vec<(String, EventPayload)> = Vec::with_capacity(2);
        if let Some(rotation) = self.rotation {
            let log = self.reader().run_log(run_id)?;
            let mut chain_key: Option<String> = None;
            for event in &log.events {
                if event.device_id.as_str() != self.device_id {
                    continue;
                }
                let key = match &event.payload {
                    EventPayload::RunStarted {
                        device_pubkey: Some(key),
                        ..
                    } => Some(key),
                    EventPayload::DeviceJoined { device_pubkey } => Some(device_pubkey),
                    EventPayload::DeviceRotated { new_pubkey, .. } => Some(new_pubkey),
                    _ => None,
                };
                if let Some(key) = key {
                    chain_key = Some(key.clone());
                }
            }
            if chain_key.as_deref() == Some(rotation.old_pubkey.as_str()) {
                let rotated = EventPayload::DeviceRotated {
                    new_pubkey: rotation.new_pubkey.clone(),
                    certificate: rotation.certificate.clone(),
                };
                pending.push((format!("{run_id}:rotate:{}", rotation.new_pubkey), rotated));
            }
        }
        pending.push((event_id.to_string(), payload));

        let mut inserted: Option<Event> = None;
        for (id, payload) in pending {
            let last: Option<(i64, String)> = self
                .tx
                .query_row(
                    "SELECT seq, hash FROM events WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
                    [run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let seq = match &last {
                Some((seq, _)) => *seq as u64 + 1,
                None => 1,
            };
            let mut event = Event {
                event_id: id,
                schema_version: SCHEMA_VERSION,
                run_id: run_id.to_string(),
                seq,
                device_id: self.device_id.to_string(),
                occurred_at,
                recorded_at: Utc::now(),
                payload,
                prev_hash: last.map(|(_, hash)| hash),
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
            inserted = Some(event);
        }
        inserted.ok_or(rusqlite::Error::InvalidQuery)
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
