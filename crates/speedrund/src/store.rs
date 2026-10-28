//! SQLite persistence. The `events` table is the source of truth and is
//! append-only, enforced by triggers. Everything else is derived.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use speedrun_core::{Event, EventPayload, SCHEMA_VERSION, hash::compute_hash, new_id};

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
];

pub struct Store {
    conn: Connection,
    device_id: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        let device_id = match conn
            .query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| r.get(0))
            .optional()?
        {
            Some(id) => id,
            None => {
                let id = new_id();
                conn.execute("INSERT INTO meta (key, value) VALUES ('device_id', ?1)", [&id])?;
                id
            }
        };
        Ok(Self { conn, device_id })
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
        };
        let out = f(&mut w)?;
        w.tx.commit()?;
        Ok(out)
    }

    pub fn reader(&self) -> Reader<'_> {
        Reader(&self.conn)
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)
            .with_context(|| format!("migration {}", i + 1))?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
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

    pub fn run_events(&self, run_id: &str) -> rusqlite::Result<Vec<Event>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT body FROM events WHERE run_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([run_id], |r| r.get::<_, String>(0))?;
        rows.map(|b| parse(&b?)).collect()
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

    pub fn runs_in_category(&self, category_id: &str, version: u32) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .0
            .prepare_cached("SELECT run_id FROM runs WHERE category_id = ?1 AND category_version = ?2")?;
        let rows = stmt.query_map(params![category_id, version], |r| r.get(0))?;
        rows.collect()
    }
}

fn parse(body: &str) -> rusqlite::Result<Event> {
    serde_json::from_str(body)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

pub struct Tx<'c> {
    tx: Transaction<'c>,
    device_id: &'c str,
}

impl Tx<'_> {
    pub fn reader(&self) -> Reader<'_> {
        Reader(&self.tx)
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
        };
        event.hash = compute_hash(&event);
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

    #[cfg(test)]
    pub fn raw(&self) -> &Connection {
        &self.tx
    }
}
