//! HTTP client for speedrund, with an on-disk spool so writes made while
//! the daemon is down are delivered later instead of lost. Every write
//! carries an event_id, so replaying the spool is safe.

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use rand_core::{OsRng, RngCore};
use rusqlite::{Connection, OptionalExtension, params};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use speedrun_core::{
    api::{ErrorBody, HealthResponse},
    live::LiveSnapshot,
    paths,
};

#[derive(Debug)]
pub enum CallError {
    /// The daemon could not be reached (not running, or no token yet).
    Unreachable(String),
    /// A daemon answered, but not for this data directory: it could not
    /// prove this data dir's token, or proved a different one.
    WrongDataDir { addr: String },
    /// The local outbox could not durably record or update a request.
    Storage(String),
    /// The daemon answered with an error status.
    Api { status: u16, message: String },
    /// The request went out, but no answer came back in time (V0.2 M8): the
    /// daemon may have recorded it. Every write carries an event id, so
    /// sending it again cannot double it.
    DeliveryUnknown(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) => write!(f, "speedrund unreachable: {m}"),
            Self::WrongDataDir { addr } => {
                write!(f, "daemon answers for another data directory ({addr})")
            }
            Self::Storage(message) => write!(f, "outbox storage error: {message}"),
            Self::Api { status, message } => write!(f, "speedrund error {status}: {message}"),
            Self::DeliveryUnknown(m) => {
                write!(
                    f,
                    "speedrund did not answer in time; it may have recorded the request: {m}"
                )
            }
        }
    }
}

impl std::error::Error for CallError {}

impl CallError {
    /// Whether this failure is worth keeping in the spool: the daemon is
    /// unreachable (no listener, or no proof it holds this data dir's
    /// token), did not answer a request it may have recorded, refused the token even after the client re-read it
    /// (401), or failed on its own side (5xx). Any other 4xx is the client's
    /// mistake and is reported as it always was.
    fn should_spool(&self) -> bool {
        match self {
            Self::Unreachable(_) | Self::WrongDataDir { .. } | Self::DeliveryUnknown(_) => true,
            Self::Api { status, .. } => *status == 401 || *status >= 500,
            Self::Storage(_) => false,
        }
    }
}

/// What became of a write that must not be lost.
pub enum Durable<T> {
    /// The daemon recorded it and answered.
    Recorded(T),
    /// It is in the spool for a later flush, for this reason.
    Spooled(SpoolReason),
}

/// Why a durable write went to the spool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpoolReason {
    /// The daemon never received it: not running, not proven, or failing.
    Undelivered,
    /// It was sent but not answered in time, so the daemon may hold it already.
    DeliveryUnknown,
}

impl SpoolReason {
    fn of(error: &CallError) -> Self {
        match error {
            CallError::DeliveryUnknown(_) => Self::DeliveryUnknown,
            CallError::Unreachable(_)
            | CallError::WrongDataDir { .. }
            | CallError::Api { .. }
            | CallError::Storage(_) => Self::Undelivered,
        }
    }
}

impl<T> Durable<T> {
    /// The daemon's answer, or `None` when the write was spooled.
    pub fn recorded(self) -> Option<T> {
        match self {
            Self::Recorded(value) => Some(value),
            Self::Spooled(_) => None,
        }
    }
}

/// Which of the data dir's tokens a request carries. The health proof always
/// uses the full token; this decides only the authenticated request itself,
/// so no caller handles a token file directly.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Credential {
    /// `$DATA_DIR/token`: the user, or a command the user ran.
    #[default]
    Full,
    /// `$DATA_DIR/agent-token`: agent tools such as the MCP server.
    Agent,
}

impl Credential {
    fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// The token this credential names. A missing file is the same
    /// unreachable daemon state a missing full token is.
    fn token(self) -> Result<String, CallError> {
        let path = match self {
            Self::Full => paths::token_path(),
            Self::Agent => paths::agent_token_path(),
        };
        fs::read_to_string(&path)
            .map(|t| t.trim().to_string())
            .map_err(|e| {
                let name = match self {
                    Self::Full => "token",
                    Self::Agent => "agent token",
                };
                CallError::Unreachable(format!("no {name} yet ({}): {e}", path.display()))
            })
    }
}

/// One spooled request. `credential` is omitted for a full-token entry, as
/// every entry written before agent credentials have it; `"agent"` marks one
/// written with the agent token, so `flush` replays it with that token.
#[derive(Clone, Serialize, Deserialize)]
struct SpoolEntry {
    path: String,
    body: Value,
    #[serde(default, skip_serializing_if = "Credential::is_full")]
    credential: Credential,
}

pub struct Client {
    agent: ureq::Agent,
    /// The token this client sends on every authenticated request. Health
    /// proofs still cover the full token.
    credential: Credential,
    /// The base URL of the daemon that last proved this data dir's token.
    /// Requests that carry the token go only here, so a body or the token
    /// never reaches an address that was not the one probed.
    verified: RefCell<Option<String>>,
}

fn spool_dir() -> PathBuf {
    paths::data_dir().join("spool")
}

/// Where the daemon listens: `SPEEDRUN_ADDR`, else `$DATA_DIR/addr`, else
/// `paths::DEFAULT_ADDR`. Naming the addr file is core's business; reading
/// it is this client's, so core stays free of I/O.
pub(crate) fn resolve_addr() -> String {
    if let Some(addr) = paths::configured_addr() {
        return addr;
    }
    let file = paths::addr_path();
    match fs::read_to_string(&file) {
        Ok(addr) => {
            let addr = addr.trim();
            if !addr.is_empty() {
                return addr.to_string();
            }
        }
        // No daemon has written one yet: fall back to the fixed address.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => report_io_error("read daemon address", &file, &e),
    }
    paths::DEFAULT_ADDR.to_string()
}

/// Safe, read-only diagnostics. Stored request bodies and server messages are never exposed.
#[derive(Serialize)]
pub(crate) struct RejectedEntry {
    id: i64,
    reason: &'static str,
    status: Option<u16>,
    rejected_at_unix: Option<i64>,
    entry_format: &'static str,
    request_kind: &'static str,
    event_id: Option<String>,
    suggested_action: &'static str,
}

#[derive(Serialize)]
pub(crate) struct RejectedPage {
    entries: Vec<RejectedEntry>,
    next_after: Option<i64>,
    legacy_unimported: u64,
}

pub(crate) fn rejected_page(dir: &Path, after: i64, limit: usize) -> anyhow::Result<RejectedPage> {
    anyhow::ensure!((1..=1000).contains(&limit), "limit must be between 1 and 1000");
    anyhow::ensure!(after >= 0, "after must be non-negative");
    let legacy = dir.join("rejected.jsonl");
    let legacy_unimported = match fs::File::open(legacy) {
        Ok(file) => BufReader::new(file)
            .split(b'\n')
            .try_fold(0u64, |n, line| line.map(|line| n + u64::from(!line.is_empty())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e.into()),
    };
    let path = dir.join("pending.sqlite3");
    if !path.exists() {
        return Ok(RejectedPage {
            entries: vec![],
            next_after: None,
            legacy_unimported,
        });
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_metadata: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='outbox_rejections')",
        [],
        |r| r.get(0),
    )?;
    let sql = if has_metadata {
        "SELECT o.id, CAST(o.entry AS BLOB), r.status, r.rejected_at FROM outbox o LEFT JOIN outbox_rejections r ON o.id=r.id WHERE o.state='rejected' AND o.id>?1 ORDER BY o.id LIMIT ?2"
    } else {
        "SELECT id, CAST(entry AS BLOB), NULL, NULL FROM outbox WHERE state='rejected' AND id>?1 ORDER BY id LIMIT ?2"
    };
    let mut stmt = db.prepare(sql)?;
    let mut entries = stmt
        .query_map(params![after, (limit + 1) as i64], |row| {
            let bytes: Vec<u8> = row.get(1)?;
            let value: Option<Value> = serde_json::from_slice(&bytes).ok();
            let entry_format = match value {
                Some(ref value) if value.get("path").and_then(Value::as_str) == Some("/legacy/rejected") => {
                    "sealed_legacy"
                }
                Some(ref value)
                    if value.get("path").and_then(Value::as_str).is_some() && value.get("body").is_some() =>
                {
                    "request"
                }
                Some(_) => "unrecognized",
                None => "malformed_or_truncated",
            };
            let request_kind = match value.as_ref().and_then(|v| v.get("path")).and_then(Value::as_str) {
                Some("/v1/prompts") => "prompt",
                Some(path)
                    if path
                        .strip_prefix("/v1/projects/")
                        .and_then(|p| p.strip_suffix("/events"))
                        .is_some_and(|p| !p.is_empty() && !p.contains('/')) =>
                {
                    "project_event"
                }
                _ => "unknown",
            };
            let event_id = value
                .as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|v| v.get("event_id"))
                .and_then(Value::as_str)
                .filter(|id| {
                    id.len() == 26
                        && id.as_bytes()[0] <= b'7'
                        && id
                            .bytes()
                            .all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c))
                })
                .map(str::to_owned);
            let suggested_action = match row.get::<_, Option<u16>>(2)? {
                Some(400 | 422) => "review_request",
                Some(403) => "check_permissions",
                Some(404) => "check_target",
                Some(409) => "check_conflict",
                Some(_) => "inspect_status",
                None => "unknown",
            };
            let status: Option<u16> = row.get(2)?;
            Ok(RejectedEntry {
                id: row.get(0)?,
                reason: if status.is_some() {
                    "http_rejection"
                } else {
                    "unknown"
                },
                status,
                rejected_at_unix: row.get(3)?,
                entry_format,
                request_kind,
                event_id,
                suggested_action,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next_after = if entries.len() > limit {
        entries.truncate(limit);
        entries.last().map(|e| e.id)
    } else {
        None
    };
    Ok(RejectedPage {
        entries,
        next_after,
        legacy_unimported,
    })
}

pub(crate) fn print_rejected(after: i64, limit: usize, json: bool) -> anyhow::Result<()> {
    let page = rejected_page(&spool_dir(), after, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&page)?);
    } else {
        for entry in &page.entries {
            println!(
                "{}: {} status={} rejected_at_unix={} format={} kind={} event_id={} action={}",
                entry.id,
                entry.reason,
                entry.status.map_or_else(|| "unknown".into(), |v| v.to_string()),
                entry
                    .rejected_at_unix
                    .map_or_else(|| "unknown".into(), |v| v.to_string()),
                entry.entry_format,
                entry.request_kind,
                entry.event_id.as_deref().unwrap_or("unknown"),
                entry.suggested_action
            );
        }
        if page.entries.is_empty() {
            println!("No rejected outbox entries.");
        }
        if let Some(id) = page.next_after {
            println!("Next page: --after {id}");
        }
        if page.legacy_unimported > 0 {
            println!(
                "Legacy rejected records awaiting import: {}",
                page.legacy_unimported
            );
        }
    }
    Ok(())
}

/// The numbers `doctor` reports from the spool directory and the failure
/// counter. Their layout is this module's business, not the caller's.
pub(crate) struct SpoolStats {
    pub(crate) spooled: u64,
    pub(crate) rejected: u64,
    pub(crate) write_failures: u64,
}

/// Count the spooled, rejected and failed-write files as they are now.
pub(crate) fn spool_stats() -> std::io::Result<SpoolStats> {
    let dir = spool_dir();
    let mut spooled = count_lines(&dir.join("pending.jsonl"))?;
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let flushing = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("flushing-"));
                if flushing && entry.file_type()?.is_file() {
                    spooled = spooled.saturating_add(count_lines(&entry.path())?);
                }
            }
        }
        // No spool directory yet: nothing has been spooled.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut rejected = count_lines(&dir.join("rejected.jsonl"))?;
    let db_path = dir.join("pending.sqlite3");
    if db_path.exists() {
        let db = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(std::io::Error::other)?;
        let (pending, refused): (i64, i64) = db.query_row(
            "SELECT COUNT(*) FILTER (WHERE state != 'rejected'), COUNT(*) FILTER (WHERE state = 'rejected') FROM outbox",
            [], |row| Ok((row.get(0)?, row.get(1)?))).map_err(std::io::Error::other)?;
        spooled = spooled.saturating_add(pending as u64);
        rejected = rejected.saturating_add(refused as u64);
    }
    Ok(SpoolStats {
        spooled,
        rejected,
        write_failures: read_count(&spool_write_failures_path())?,
    })
}

fn count_lines(path: &Path) -> std::io::Result<u64> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    if bytes.is_empty() {
        return Ok(0);
    }
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count() as u64;
    if bytes.last() == Some(&b'\n') {
        Ok(newlines)
    } else {
        Ok(newlines + 1)
    }
}

fn read_count(path: &Path) -> std::io::Result<u64> {
    match fs::read_to_string(path) {
        Ok(text) => text.trim().parse::<u64>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is not a decimal count: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

fn spool_write_failures_path() -> PathBuf {
    paths::data_dir().join("spool-write-failures")
}

/// Header the daemon reads to exempt a spooled write from its last-event
/// clock bound. Only `flush_spool` marks a delivery this way.
const REPLAY_HEADER: &str = "X-Speedrun-Replay";

/// How a request goes out: a fresh write, or a spool replay, which carries
/// [`REPLAY_HEADER`] and may be bounded by the flush's deadline.
#[derive(Clone, Copy)]
enum Delivery {
    Fresh,
    Replay { deadline: Option<Instant> },
}

/// How long a durable write may spend draining the backlog after its own
/// event has been attempted. Hooks must return to their host quickly, so an
/// entry this does not reach stays spooled for a later flush.
const FLUSH_BUDGET: Duration = Duration::from_secs(3);

impl Client {
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(5), Credential::Full)
    }

    /// For milestone claims: the daemon checks evidence before answering,
    /// which can take several network round trips.
    pub fn patient() -> Self {
        Self::with_timeout(Duration::from_secs(120), Credential::Full)
    }

    /// For the MCP server and `speedrun tool`: authenticated requests carry
    /// the agent token, while the health proof still covers the full token.
    pub fn agent() -> Self {
        Self::with_timeout(Duration::from_secs(5), Credential::Agent)
    }

    /// The patient agent client, for a claim whose evidence the daemon checks.
    pub fn agent_patient() -> Self {
        Self::with_timeout(Duration::from_secs(120), Credential::Agent)
    }

    fn with_timeout(timeout: Duration, credential: Credential) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_millis(500))
                .timeout(timeout)
                .build(),
            credential,
            verified: RefCell::new(None),
        }
    }

    /// The daemon that proved the token in `ready`. Nothing that carries the
    /// token is sent before one has.
    fn verified_base(&self) -> Result<String, CallError> {
        self.verified
            .borrow()
            .clone()
            .ok_or_else(|| CallError::Unreachable("no daemon has proved this data dir's token".into()))
    }

    /// Send one request with `credential`, re-reading that token and retrying
    /// once on 401: a daemon that restarted wrote a fresh token, and the
    /// first read may predate it.
    fn send<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        delivery: Delivery,
        credential: Credential,
    ) -> Result<T, CallError> {
        let mut retried = false;
        loop {
            if let Delivery::Replay {
                deadline: Some(deadline),
            } = delivery
                && Instant::now() >= deadline
            {
                return Err(CallError::DeliveryUnknown("flush deadline elapsed".into()));
            }
            let token = credential.token()?;
            match self.attempt(method, path, body, delivery, &token) {
                Err(CallError::Api { status: 401, .. }) if !retried => retried = true,
                other => return other,
            }
        }
    }

    fn attempt<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        delivery: Delivery,
        token: &str,
    ) -> Result<T, CallError> {
        let mut req = self
            .agent
            .request(method, &format!("{}{path}", self.verified_base()?))
            .set("Authorization", &format!("Bearer {token}"));
        if let Delivery::Replay { deadline } = delivery {
            req = req.set(REPLAY_HEADER, "1");
            // A replay may not outlast the flush's budget: a slow daemon times out
            // and the entry stays spooled, rather than holding up the hook.
            if let Some(deadline) = deadline {
                req = req.timeout(deadline.saturating_duration_since(Instant::now()));
            }
        }
        let res = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        match res {
            Ok(r) => r
                .into_json()
                .map_err(|e| CallError::DeliveryUnknown(format!("unreadable response: {e}"))),
            Err(ureq::Error::Status(status, r)) => {
                let message = match r.into_json::<ErrorBody>() {
                    Ok(body) => body.error,
                    Err(e) => format!("unreadable error body: {e}"),
                };
                Err(CallError::Api { status, message })
            }
            Err(ureq::Error::Transport(t)) => Err(transport_error(&t)),
        }
    }

    /// Prove the daemon holds this data dir's token before a body is sent: a
    /// fresh nonce, then `digest::token_proof(token, nonce)` back from
    /// `/v1/health`. A missing or wrong proof is a daemon for another data
    /// directory, so callers spool and send nothing.
    ///
    /// The probe itself carries no token and is not retried on 401: it is
    /// what decides whether the daemon may see the token at all. On success
    /// the probed address becomes the only one requests are sent to.
    pub(crate) fn verify_health(&self) -> Result<HealthResponse, CallError> {
        let addr = resolve_addr();
        let base = format!("http://{addr}");
        let nonce = speedrun_core::hash::sha256_hex(speedrun_core::new_id().as_bytes());
        let health: HealthResponse = match self.agent.get(&format!("{base}/v1/health?nonce={nonce}")).call() {
            Ok(response) => response
                .into_json()
                .map_err(|e| CallError::Unreachable(format!("unreadable health answer from {base}: {e}")))?,
            Err(ureq::Error::Status(status, _)) => {
                return Err(CallError::Unreachable(format!(
                    "health check at {base} answered {status}"
                )));
            }
            Err(ureq::Error::Transport(t)) => return Err(CallError::Unreachable(t.to_string())),
        };
        let Some(proof) = health.proof.as_deref() else {
            return Err(CallError::WrongDataDir { addr: addr.clone() });
        };
        let expected = speedrun_core::digest::token_proof(&Credential::Full.token()?, &nonce);
        let dir = paths::data_dir()
            .canonicalize()
            .map_err(|e| CallError::Unreachable(format!("resolving data directory: {e}")))?;
        let expected_dir = speedrun_core::hash::sha256_hex(dir.as_os_str().as_encoded_bytes());
        if proof == expected.as_str() && health.data_dir_id == expected_dir {
            *self.verified.borrow_mut() = Some(base);
            Ok(health)
        } else {
            Err(CallError::WrongDataDir { addr })
        }
    }

    /// Make sure the daemon is up and has proved this data dir's token
    /// (starting it if needed). Delivery is deliberately separate: a durable
    /// write sends its own event before the backlog, and `flush_spool` owns
    /// how long that backlog may take.
    pub fn ready(&self) -> Result<(), CallError> {
        self.with_autostart(|| self.verify_health().map(|_| ()))
    }

    /// Read from an already verified daemon without starting one.
    pub(crate) fn get_existing<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.send("GET", path, None, Delivery::Fresh, self.credential)
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.ready()?;
        self.send("GET", path, None, Delivery::Fresh, self.credential)
    }

    pub fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.ready()?;
        self.send("DELETE", path, None, Delivery::Fresh, self.credential)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, CallError> {
        self.ready()?;
        self.send("POST", path, Some(body), Delivery::Fresh, self.credential)
    }

    /// POST that must not be lost. The fresh event goes out before the spool:
    /// verify the daemon, attempt this write, then flush the backlog for at
    /// most [`FLUSH_BUDGET`]. Entries the budget does not reach stay spooled,
    /// in order. When the daemon has not proved the token, the write is
    /// spooled without a flush because there is no proven base to deliver to.
    pub fn post_durable<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<Durable<T>, CallError> {
        // Commit before sending: a crash or an unreadable response can never
        // leave an accepted request without a durable retry record.
        let owner = speedrun_core::new_id();
        let entry = SpoolEntry {
            path: path.to_string(),
            body: body.clone(),
            credential: self.credential,
        };
        let db = open_outbox().map_err(storage_error)?;
        let id = enqueue(&db, &entry, Some(&owner)).map_err(storage_error)?;
        let outcome = match self.ready() {
            Ok(()) => self.send("POST", path, Some(body), Delivery::Fresh, self.credential),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(value) => {
                if let Err(error) = finish(&db, id, &owner, false) {
                    report_storage_error(&error);
                }
                let _ = self.flush_spool(Some(Instant::now() + FLUSH_BUDGET));
                Ok(Durable::Recorded(value))
            }
            Err(error) if error.should_spool() => {
                if let Err(storage) = release(&db, id, &owner) {
                    report_storage_error(&storage);
                }
                Ok(Durable::Spooled(SpoolReason::of(&error)))
            }
            Err(error) => {
                if let Err(storage) = finish_rejected(&db, id, &owner, &error, None) {
                    report_storage_error(&storage);
                }
                Err(error)
            }
        }
    }

    /// The daemon `ready` verified.
    pub fn base_url(&self) -> Result<String, CallError> {
        self.verified_base()
    }

    /// Ask the daemon at the verified base to stop. The full token goes only
    /// to a daemon that proved it, and a failed request is reported rather
    /// than treated as a stopped daemon.
    pub(crate) fn shutdown(&self) -> Result<(), CallError> {
        let base = self.verified_base()?;
        let token = Credential::Full.token()?;
        let res = self
            .agent
            .request("POST", &format!("{base}/v1/shutdown"))
            .set("Authorization", &format!("Bearer {token}"))
            .call();
        match res {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(status, r)) => {
                let message = match r.into_json::<ErrorBody>() {
                    Ok(body) => body.error,
                    Err(e) => format!("unreadable error body: {e}"),
                };
                Err(CallError::Api { status, message })
            }
            Err(ureq::Error::Transport(t)) => Err(CallError::Unreachable(t.to_string())),
        }
    }

    /// Follow the live stream with the read-only display token, calling
    /// `on_snapshot` for each snapshot. Returns when the connection ends.
    pub fn stream_live(
        &self,
        since_hours: u32,
        mut on_snapshot: impl FnMut(LiveSnapshot),
    ) -> Result<(), CallError> {
        let token = fs::read_to_string(paths::display_token_path())
            .map(|t| t.trim().to_string())
            .map_err(|_| CallError::Unreachable("no display token yet; has speedrund ever run?".into()))?;
        // No overall timeout: the stream is meant to stay open.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(500))
            .build();
        let resp = agent
            .get(&format!(
                "{}/v1/live/stream?since_hours={since_hours}",
                self.verified_base()?
            ))
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(status, _) => CallError::Api {
                    status,
                    message: "stream refused".into(),
                },
                ureq::Error::Transport(t) => CallError::Unreachable(t.to_string()),
            })?;
        let (mut event, mut data) = (String::new(), String::new());
        for line in BufReader::new(resp.into_reader()).lines() {
            let line = line.map_err(|e| CallError::Unreachable(e.to_string()))?;
            if line.is_empty() {
                if event == "snapshot"
                    && let Ok(snap) = serde_json::from_str(&data)
                {
                    on_snapshot(snap);
                }
                event.clear();
                data.clear();
            } else if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        Err(CallError::Unreachable("stream closed".into()))
    }

    /// On an unreachable daemon, try to start it once and retry.
    fn with_autostart<T>(&self, f: impl Fn() -> Result<T, CallError>) -> Result<T, CallError> {
        match f() {
            Err(CallError::Unreachable(_)) if start_daemon(false) => {
                let deadline = Instant::now() + Duration::from_millis(1500);
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    match f() {
                        Err(CallError::Unreachable(_)) if Instant::now() < deadline => continue,
                        other => return other,
                    }
                }
            }
            other => other,
        }
    }

    /// Claim one outbox row at a time. Expired leases are recoverable after
    /// crashes; event IDs make an uncertain delivery safe to replay.
    pub fn flush_spool(&self, deadline: Option<Instant>) -> Result<usize, CallError> {
        let Some(db) = prepare_flush(&spool_dir(), deadline).map_err(storage_error)? else {
            return Ok(0);
        };
        let owner = speedrun_core::new_id();
        let mut delivered = 0;
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            set_busy_deadline(&db, deadline).map_err(storage_error)?;
            let Some((id, entry)) = claim(&db, &owner).map_err(storage_error)? else {
                break;
            };
            let outcome = self.send::<Value>(
                "POST",
                &entry.path,
                Some(&entry.body),
                Delivery::Replay { deadline },
                entry.credential,
            );
            match outcome {
                Ok(_) => {
                    finish_with_deadline(&db, id, &owner, false, deadline).map_err(storage_error)?;
                    delivered += 1;
                }
                Err(error) if error.should_spool() => {
                    set_busy_deadline(&db, deadline).map_err(storage_error)?;
                    release(&db, id, &owner).map_err(storage_error)?;
                    break;
                }
                Err(error) => finish_rejected(&db, id, &owner, &error, deadline).map_err(storage_error)?,
            }
        }
        Ok(delivered)
    }
}

/// A transport failure on an authenticated request. One that ends before the
/// request is sent (no connection, a bad address) means the daemon never saw
/// it. Any other (a timeout or reset while sending or awaiting the answer, an
/// answer that does not parse) leaves unknown whether the daemon recorded it.
fn transport_error(t: &ureq::Transport) -> CallError {
    use ureq::ErrorKind;
    match t.kind() {
        ErrorKind::InvalidUrl
        | ErrorKind::UnknownScheme
        | ErrorKind::Dns
        | ErrorKind::InsecureRequestHttpsOnly
        | ErrorKind::ConnectionFailed
        | ErrorKind::InvalidProxyUrl
        | ErrorKind::ProxyConnect
        | ErrorKind::ProxyUnauthorized => CallError::Unreachable(t.to_string()),
        ErrorKind::TooManyRedirects
        | ErrorKind::BadStatus
        | ErrorKind::BadHeader
        | ErrorKind::Io
        | ErrorKind::HTTP => CallError::DeliveryUnknown(t.to_string()),
    }
}

// SQLite holds the only live copy until acknowledgement or durable rejection.
// FULL synchronous mode and a transaction around each state transition protect
// against process and power failure. A lease is longer than any request timeout.
const LEASE_SECONDS: i64 = 300;
const REJECTED_LIMIT: i64 = 4 * 1024 * 1024;

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
pub(crate) fn test_enqueue_prompt(spool: &Path, id: &str) -> anyhow::Result<()> {
    let db = open_outbox_at(spool, None)?;
    let entry = SpoolEntry {
        path: "/v1/prompts".into(),
        body: serde_json::json!({"event_id": id, "content": "recovery fixture"}),
        credential: Credential::Full,
    };
    enqueue(&db, &entry, None)?;
    Ok(())
}

fn open_outbox() -> anyhow::Result<Connection> {
    open_outbox_at(&spool_dir(), None)
}

// A bounded hook flush never waits for the legacy import lock or performs an
// unbounded file import. Explicit flushes still import before delivering.
fn prepare_flush(dir: &Path, deadline: Option<Instant>) -> anyhow::Result<Option<Connection>> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Ok(None);
    }
    let db = open_outbox_at(dir, deadline)?;
    if deadline.is_none() {
        import_legacy(&db, dir)?;
    }
    Ok(Some(db))
}

fn open_outbox_at(dir: &Path, deadline: Option<Instant>) -> anyhow::Result<Connection> {
    fs::create_dir_all(dir)?;
    let path = dir.join("pending.sqlite3");
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    open_outbox_path_with_deadline(&path, deadline)
}

#[cfg(test)]
fn open_outbox_path(path: &Path) -> anyhow::Result<Connection> {
    open_outbox_path_with_deadline(path, None)
}

fn set_busy_deadline(db: &Connection, deadline: Option<Instant>) -> anyhow::Result<()> {
    let timeout = deadline.map_or(Duration::from_secs(2), |d| {
        d.saturating_duration_since(Instant::now())
            .min(Duration::from_secs(2))
    });
    db.busy_timeout(timeout)?;
    Ok(())
}

fn open_outbox_path_with_deadline(path: &Path, deadline: Option<Instant>) -> anyhow::Result<Connection> {
    let db = Connection::open(path)?;
    for statement in ["PRAGMA synchronous=FULL;", "PRAGMA secure_delete=ON;"] {
        set_busy_deadline(&db, deadline)?;
        db.execute_batch(statement)?;
    }
    // Once initialized, opening an outbox needs no write lock. This matters
    // when another process is already claiming or acknowledging a message.
    if initialized_outbox_is_ready(&db, deadline)? {
        return Ok(db);
    }
    for statement in [
        "PRAGMA auto_vacuum=FULL;",
        "CREATE TABLE IF NOT EXISTS outbox (id INTEGER PRIMARY KEY, entry TEXT NOT NULL, state TEXT NOT NULL, owner TEXT, lease_until INTEGER, source TEXT UNIQUE);",
        "CREATE TABLE IF NOT EXISTS outbox_rejections (id INTEGER PRIMARY KEY, status INTEGER, rejected_at INTEGER NOT NULL);",
        "CREATE TABLE IF NOT EXISTS outbox_ids (id INTEGER PRIMARY KEY CHECK(id=1), highwater INTEGER NOT NULL); INSERT OR IGNORE INTO outbox_ids SELECT 1, COALESCE(MAX(id),0) FROM outbox;",
        "CREATE TRIGGER IF NOT EXISTS outbox_track_id AFTER INSERT ON outbox BEGIN UPDATE outbox_ids SET highwater=MAX(highwater,NEW.id) WHERE id=1; END;",
    ] {
        set_busy_deadline(&db, deadline)?;
        db.execute_batch(statement)?;
    }
    initialize_outbox_crypto(&db, path)?;
    Ok(db)
}

// A partially initialized or interrupted migration must take the ordinary
// initialization path. The fast path checks both the schema and the encrypted
// key marker before accepting an existing outbox.
fn initialized_outbox_is_ready(db: &Connection, deadline: Option<Instant>) -> anyhow::Result<bool> {
    set_busy_deadline(db, deadline)?;
    let objects: i64 = db.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE (type='table' AND name IN ('outbox','outbox_rejections','outbox_ids','outbox_crypto')) OR (type='trigger' AND name='outbox_track_id')",
        [],
        |row| row.get(0),
    )?;
    if objects != 5 {
        return Ok(false);
    }
    set_busy_deadline(db, deadline)?;
    let highwater_present: bool =
        db.query_row("SELECT EXISTS(SELECT 1 FROM outbox_ids WHERE id=1)", [], |row| {
            row.get(0)
        })?;
    set_busy_deadline(db, deadline)?;
    let verifier: Option<(String, bool)> = db
        .query_row(
            "SELECT verifier,scrub_pending FROM outbox_crypto WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((verifier, false)) = verifier else {
        return Ok(false);
    };
    if !highwater_present {
        return Ok(false);
    }
    verify_outbox_key(db, &verifier)?;
    Ok(true)
}

fn verify_outbox_key(db: &Connection, verifier: &str) -> anyhow::Result<()> {
    let key = read_outbox_key(db)?;
    let envelope: Value = serde_json::from_str(verifier)?;
    anyhow::ensure!(
        open_sealed(&key, &envelope, b"outbox-key-v1")? == b"speedrun outbox",
        "invalid outbox key"
    );
    Ok(())
}

// The key is independent of daemon keys: retain it with the outbox during recovery.
fn outbox_key_path(db: &Connection) -> anyhow::Result<PathBuf> {
    let path = db
        .path()
        .ok_or_else(|| anyhow::anyhow!("outbox has no filesystem path"))?;
    Ok(Path::new(path)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("outbox has no directory"))?
        .join("outbox.key"))
}

fn read_outbox_key(db: &Connection) -> anyhow::Result<[u8; 32]> {
    let path = outbox_key_path(db)?;
    let bytes =
        fs::read(&path).map_err(|e| anyhow::anyhow!("cannot read outbox key {}: {e}", path.display()))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid outbox key length"))
}

fn seal(key: &[u8; 32], plaintext: &[u8], aad: &[u8]) -> anyhow::Result<Value> {
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new(key.into());
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plaintext, aad })
        .map_err(|_| anyhow::anyhow!("outbox encryption failed"))?;
    Ok(serde_json::json!({"outbox_ciphertext": hex::encode(ciphertext), "nonce": hex::encode(nonce)}))
}

fn open_sealed(key: &[u8; 32], envelope: &Value, aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ciphertext = hex::decode(
        envelope
            .get("outbox_ciphertext")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("invalid outbox ciphertext"))?,
    )?;
    let nonce = hex::decode(
        envelope
            .get("nonce")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("invalid outbox nonce"))?,
    )?;
    anyhow::ensure!(nonce.len() == 24, "invalid outbox nonce length");
    XChaCha20Poly1305::new(key.into())
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("outbox authentication failed"))
}

fn entry_aad(entry: &SpoolEntry) -> anyhow::Result<Vec<u8>> {
    let id = entry
        .body
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("prompt outbox entry has no event id"))?;
    Ok(serde_json::to_vec(&(&entry.path, id))?)
}

fn protect_entry(db: &Connection, entry: &SpoolEntry) -> anyhow::Result<SpoolEntry> {
    let mut protected = entry.clone();
    if entry.path == "/v1/prompts"
        && entry.body.get("metadata_only").is_none_or(Value::is_null)
        && let Some(content) = entry.body.get("content").and_then(Value::as_str)
    {
        let aad = entry_aad(entry)?;
        protected.body["content"] = seal(&read_outbox_key(db)?, content.as_bytes(), &aad)?;
    }
    Ok(protected)
}

fn unprotect_entry(db: &Connection, mut entry: SpoolEntry) -> anyhow::Result<SpoolEntry> {
    if let Some(content) = entry.body.get("content")
        && (content.get("outbox_ciphertext").is_some()
            || (entry.path == "/v1/prompts" && entry.body.get("metadata_only").is_none_or(Value::is_null)))
    {
        anyhow::ensure!(content.is_object(), "plaintext prompt found in encrypted outbox");
        let plaintext = open_sealed(&read_outbox_key(db)?, content, &entry_aad(&entry)?)?;
        entry.body["content"] = Value::String(String::from_utf8(plaintext)?);
    }
    Ok(entry)
}

/// Validate an existing copied outbox without creating or modifying files.
pub(crate) fn validate_existing_outbox(path: &Path) -> anyhow::Result<()> {
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let columns: Vec<String> = {
        let mut statement = db.prepare("PRAGMA table_info(outbox)")?;
        let rows = statement.query_map([], |row| row.get(1))?;
        rows.collect::<Result<_, _>>()?
    };
    for required in ["id", "entry", "state", "owner", "lease_until", "source"] {
        anyhow::ensure!(
            columns.iter().any(|column| column == required),
            "invalid outbox schema: missing {required}"
        );
    }
    let initialized: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='outbox_crypto')",
        [],
        |row| row.get(0),
    )?;
    if initialized {
        let verifier: String = db
            .query_row("SELECT verifier FROM outbox_crypto WHERE id=1", [], |row| {
                row.get(0)
            })
            .map_err(|_| anyhow::anyhow!("initialized outbox has no valid key verifier"))?;
        let key = read_outbox_key(&db)?;
        let envelope: Value = serde_json::from_str(&verifier)?;
        anyhow::ensure!(
            open_sealed(&key, &envelope, b"outbox-key-v1")? == b"speedrun outbox",
            "invalid outbox key"
        );
    }
    // Rejected diagnostics may have been deliberately truncated; they are never replayed.
    let mut statement = db.prepare("SELECT entry FROM outbox WHERE state IN ('pending','leased')")?;
    let entries = statement.query_map([], |row| row.get::<_, String>(0))?;
    for encoded in entries {
        let entry: SpoolEntry = serde_json::from_str(&encoded?)?;
        if entry.path == "/v1/prompts" {
            entry_aad(&entry)?;
            let content = entry
                .body
                .get("content")
                .ok_or_else(|| anyhow::anyhow!("prompt outbox entry has no content"))?;
            if entry
                .body
                .get("metadata_only")
                .is_some_and(|value| !value.is_null())
            {
                anyhow::ensure!(
                    content.as_str() == Some(""),
                    "metadata-only outbox entry has nonempty content"
                );
            } else if initialized {
                anyhow::ensure!(content.is_object(), "plaintext prompt found in encrypted outbox");
            } else {
                anyhow::ensure!(content.is_string(), "legacy prompt has invalid content");
            }
        }
        if initialized {
            unprotect_entry(&db, entry)?;
        } else {
            // A legacy outbox is eligible for encryption during migration, but
            // encrypted records without a verifier cannot be authenticated.
            anyhow::ensure!(
                entry
                    .body
                    .get("content")
                    .is_none_or(|value| value.get("outbox_ciphertext").is_none()),
                "encrypted outbox entry has no verifier"
            );
        }
    }
    Ok(())
}

fn initialize_outbox_crypto(db: &Connection, path: &Path) -> anyhow::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS outbox_crypto (id INTEGER PRIMARY KEY CHECK(id=1), verifier TEXT NOT NULL, scrub_pending INTEGER NOT NULL);")?;
    let tx = db.unchecked_transaction()?;
    // Obtain a write lock before inspecting the marker or creating a key.
    tx.execute("UPDATE outbox_crypto SET id=id WHERE id=1", [])?;
    let verifier: Option<(String, bool)> = tx
        .query_row(
            "SELECT verifier,scrub_pending FROM outbox_crypto WHERE id=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((verifier, scrub_pending)) = verifier {
        verify_outbox_key(&tx, &verifier)?;
        tx.commit()?;
        if scrub_pending {
            scrub_migrated_outbox(db)?;
        }
        return Ok(());
    }
    let key_path = outbox_key_path(&tx)?;
    if !key_path.exists() {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)?;
        file.write_all(&key)?;
        file.sync_all()?;
        fs::File::open(
            path.parent()
                .ok_or_else(|| anyhow::anyhow!("outbox has no directory"))?,
        )?
        .sync_all()?;
    }
    let key = read_outbox_key(&tx)?;
    let rows: Vec<(i64, String)> = {
        let mut stmt = tx.prepare("SELECT id,entry FROM outbox")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (id, encoded) in rows {
        let entry: SpoolEntry = serde_json::from_str(&encoded)?;
        let protected = protect_entry(&tx, &entry)?;
        tx.execute(
            "UPDATE outbox SET entry=?1 WHERE id=?2",
            params![serde_json::to_string(&protected)?, id],
        )?;
    }
    let verifier = seal(&key, b"speedrun outbox", b"outbox-key-v1")?;
    tx.execute(
        "INSERT INTO outbox_crypto(id,verifier,scrub_pending) VALUES(1,?1,1)",
        [serde_json::to_string(&verifier)?],
    )?;
    tx.commit()?;
    scrub_migrated_outbox(db)
}

fn scrub_migrated_outbox(db: &Connection) -> anyhow::Result<()> {
    // A crash leaves scrub_pending set, so the next opener repeats this work.
    let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    anyhow::ensure!(busy == 0, "outbox WAL checkpoint is busy");
    db.execute_batch("VACUUM;")?;
    let busy: i64 = db.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    anyhow::ensure!(busy == 0, "outbox WAL checkpoint is busy");
    db.execute("UPDATE outbox_crypto SET scrub_pending=0 WHERE id=1", [])?;
    Ok(())
}

fn enqueue(db: &Connection, entry: &SpoolEntry, owner: Option<&str>) -> anyhow::Result<i64> {
    let encoded = serde_json::to_string(&protect_entry(db, entry)?)?;
    db.execute(
        "INSERT INTO outbox(id,entry,state,owner,lease_until) VALUES ((SELECT highwater+1 FROM outbox_ids WHERE id=1),?1,?2,?3,?4)",
        params![
            encoded,
            if owner.is_some() { "leased" } else { "pending" },
            owner,
            owner.map(|_| now_seconds() + LEASE_SECONDS)
        ],
    )?;
    Ok(db.last_insert_rowid())
}

fn claim(db: &Connection, owner: &str) -> anyhow::Result<Option<(i64, SpoolEntry)>> {
    // UPDATE ... RETURNING makes selection and ownership indivisible across processes.
    let row: Option<(i64, String)> = db.query_row(
        "UPDATE outbox SET state='leased',owner=?1,lease_until=?2 WHERE id=(SELECT id FROM outbox WHERE state='pending' OR (state='leased' AND lease_until<=?3) ORDER BY id LIMIT 1) RETURNING id,entry",
        params![owner, now_seconds()+LEASE_SECONDS, now_seconds()], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
    row.map(|(id, entry)| {
        let entry: SpoolEntry = serde_json::from_str(&entry)?;
        Ok((id, unprotect_entry(db, entry)?))
    })
    .transpose()
}

fn release(db: &Connection, id: i64, owner: &str) -> anyhow::Result<()> {
    let changed = db.execute(
        "UPDATE outbox SET state='pending',owner=NULL,lease_until=NULL WHERE id=?1 AND owner=?2 AND state='leased'",
        params![id, owner],
    )?;
    anyhow::ensure!(changed == 1, "outbox lease was lost");
    Ok(())
}

fn finish(db: &Connection, id: i64, owner: &str, rejected: bool) -> anyhow::Result<()> {
    finish_with_deadline(db, id, owner, rejected, None)
}

fn finish_with_deadline(
    db: &Connection,
    id: i64,
    owner: &str,
    rejected: bool,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    finish_outbox(db, id, owner, rejected, None, deadline)
}

fn finish_rejected(
    db: &Connection,
    id: i64,
    owner: &str,
    error: &CallError,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    let status = match error {
        CallError::Api { status, .. } => Some(*status),
        _ => None,
    };
    finish_outbox(db, id, owner, true, status, deadline)
}

fn finish_outbox(
    db: &Connection,
    id: i64,
    owner: &str,
    rejected: bool,
    status: Option<u16>,
    deadline: Option<Instant>,
) -> anyhow::Result<()> {
    set_busy_deadline(db, deadline)?;
    let tx = db.unchecked_transaction()?;
    if rejected {
        let changed = tx.execute(
            "UPDATE outbox SET state='rejected',entry=substr(CAST(entry AS BLOB),1,4194304),owner=NULL,lease_until=NULL WHERE id=?1 AND owner=?2 AND state='leased'",
            params![id, owner],
        )?;
        anyhow::ensure!(changed == 1, "outbox lease was lost");
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        tx.execute(
            "INSERT OR REPLACE INTO outbox_rejections(id,status,rejected_at) VALUES (?1,?2,?3)",
            params![id, status, now],
        )?;
        // Keep the newest bounded diagnostic records, including at most one oversized entry.
        set_busy_deadline(&tx, deadline)?;
        tx.execute("DELETE FROM outbox WHERE state='rejected' AND id NOT IN (SELECT id FROM (SELECT id, SUM(length(CAST(entry AS BLOB))) OVER (ORDER BY id DESC) AS total, ROW_NUMBER() OVER (ORDER BY id DESC) AS n FROM outbox WHERE state='rejected') WHERE total<=?1 OR n=1)", [REJECTED_LIMIT])?;
        tx.execute(
            "DELETE FROM outbox_rejections WHERE id NOT IN (SELECT id FROM outbox WHERE state='rejected')",
            [],
        )?;
    } else {
        let changed = tx.execute(
            "DELETE FROM outbox WHERE id=?1 AND owner=?2 AND state='leased'",
            params![id, owner],
        )?;
        anyhow::ensure!(changed == 1, "outbox lease was lost");
    }
    set_busy_deadline(&tx, deadline)?;
    tx.commit()?;
    Ok(())
}

fn report_storage_error(error: &anyhow::Error) {
    report_spool_write_error(
        "update outbox",
        &spool_dir().join("pending.sqlite3"),
        &std::io::Error::other(error.to_string()),
    );
}

fn storage_error(error: anyhow::Error) -> CallError {
    // Contention leaves the row intact and is not a failed durable write.
    let busy = error.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
        matches!(
            e.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    });
    if !busy {
        report_storage_error(&error);
    }
    CallError::Storage(error.to_string())
}

/// Import old files transactionally. A source/line identity prevents duplicates
/// if a process dies after commit but before removing the source file.
fn import_legacy(db: &Connection, dir: &Path) -> anyhow::Result<()> {
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("import.lock"))?;
    lock.lock()?;
    for item in fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        if name != "pending.jsonl" && name != "rejected.jsonl" && !name.starts_with("flushing-") {
            continue;
        }
        let bytes = fs::read(item.path())?;
        let tx = db.unchecked_transaction()?;
        for (number, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let source = format!("{name}:{number}:{}", speedrun_core::hash::sha256_hex(line));
            if name == "rejected.jsonl" {
                let entry = SpoolEntry {
                    path: "/legacy/rejected".into(),
                    body: serde_json::json!({"sealed_legacy": seal(&read_outbox_key(&tx)?, line, source.as_bytes())?, "source": source}),
                    credential: Credential::Full,
                };
                tx.execute(
                    "INSERT OR IGNORE INTO outbox(id,entry,state,source) VALUES ((SELECT highwater+1 FROM outbox_ids WHERE id=1),?1,'rejected',?2)",
                    params![serde_json::to_string(&entry)?, source],
                )?;
            } else if let Ok(entry) = serde_json::from_slice::<SpoolEntry>(line) {
                tx.execute(
                    "INSERT OR IGNORE INTO outbox(id,entry,state,source) VALUES ((SELECT highwater+1 FROM outbox_ids WHERE id=1),?1,'pending',?2)",
                    params![serde_json::to_string(&protect_entry(&tx, &entry)?)?, source],
                )?;
            } else {
                // Preserve an unreadable record without blocking later lines or
                // copying potentially sensitive bytes into plaintext diagnostics.
                let entry = SpoolEntry {
                    path: "/legacy/rejected".into(),
                    body: serde_json::json!({"sealed_legacy": seal(&read_outbox_key(&tx)?, line, source.as_bytes())?, "source": source}),
                    credential: Credential::Full,
                };
                tx.execute(
                    "INSERT OR IGNORE INTO outbox(id,entry,state,source) VALUES ((SELECT highwater+1 FROM outbox_ids WHERE id=1),?1,'rejected',?2)",
                    params![serde_json::to_string(&entry)?, source],
                )?;
            }
        }
        tx.commit()?;
        fs::remove_file(item.path())?;
        fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
fn append_line_result(file: &Path, line: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(file)?;
    f.write_all(line)?;
    f.write_all(b"\n")
}

fn report_io_error(action: &str, file: &Path, e: &std::io::Error) {
    if std::env::var_os("SPEEDRUN_DEBUG").is_some() {
        eprintln!("speedrun: could not {action} {}: {e}", file.display());
    }
}

/// Report a failed spool or rejected-log write and remember it in the data
/// dir's counter, so `doctor` can show damage the daemon never saw.
fn report_spool_write_error(action: &str, file: &Path, e: &std::io::Error) {
    report_io_error(action, file, e);
    if let Err(count_error) = increment_spool_write_failures() {
        let counter = spool_write_failures_path();
        report_io_error("record spool write failure", &counter, &count_error);
    }
}

fn increment_spool_write_failures() -> std::io::Result<()> {
    let path = spool_write_failures_path();
    let next = read_count(&path)?.saturating_add(1);
    let temp = path.with_file_name(format!(".spool-write-failures-{}.tmp", speedrun_core::new_id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    write!(file, "{next}")?;
    file.sync_all()?;
    fs::rename(&temp, &path)
}

/// A capped log of refused spool entries. `append` is the only way to write
/// to it, and it enforces the cap, so callers cannot leave the file unbounded.
#[cfg(test)]
struct RejectedLog {
    file: PathBuf,
    max_bytes: u64,
}

#[cfg(test)]
impl RejectedLog {
    fn with_cap(file: PathBuf, max_bytes: u64) -> Self {
        Self { file, max_bytes }
    }

    /// Append and trim under one exclusive lock, so a trim never writes back
    /// a copy that lacks a line another process appended after its read.
    fn try_append(&self, line: &[u8]) -> std::io::Result<()> {
        let _lock = self.lock()?;
        let current = match fs::metadata(&self.file) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let max_line = self.max_bytes.saturating_sub(1) as usize;
        let line = &line[..line.len().min(max_line)];
        if current.saturating_add(line.len() as u64 + 1) > self.max_bytes {
            self.trim_to_newest()?;
            if fs::metadata(&self.file)
                .map(|m| m.len())
                .unwrap_or(0)
                .saturating_add(line.len() as u64 + 1)
                > self.max_bytes
            {
                // The new line uses the whole budget: drop the previous diagnostic copy.
                fs::write(&self.file, [])?;
            }
        }
        append_line_result(&self.file, line)
    }

    /// The lock file beside the log. Held until the returned file is dropped.
    fn lock(&self) -> std::io::Result<fs::File> {
        let mut path = self.file.clone().into_os_string();
        path.push(".lock");
        let path = PathBuf::from(path);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        lock.lock()?;
        Ok(lock)
    }

    /// Keep the newest entries that fit in half the cap; the rest of the
    /// headroom amortizes trimming over several appends. The trimmed copy
    /// replaces the log by rename, so a crash mid-trim leaves the old log.
    /// Called with the lock held.
    fn trim_to_newest(&self) -> std::io::Result<()> {
        // Bytes, not text: a refused line need not be UTF-8.
        let text = match fs::read(&self.file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if text.is_empty() {
            return Ok(());
        }
        let keep = self.max_bytes / 2;
        let mut total = 0u64;
        let mut kept: Vec<&[u8]> = Vec::new();
        for line in text.split(|&b| b == b'\n').rev() {
            // The empty piece after the final newline is not a line.
            if line.is_empty() {
                continue;
            }
            let len = line.len() as u64 + 1;
            if !kept.is_empty() && total.saturating_add(len) > keep {
                break;
            }
            total = total.saturating_add(len);
            kept.push(line);
        }
        kept.reverse();
        let mut out = kept.join(&b'\n');
        out.push(b'\n');
        let temp = self
            .file
            .with_file_name(format!(".rejected-{}.tmp", speedrun_core::new_id()));
        let mut copy = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        copy.write_all(&out)?;
        fs::rename(&temp, &self.file)
    }
}

/// How often a wait for a daemon looks again.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Check process existence without requiring an external process utility.
pub(crate) fn pid_exited(pid: u32) -> std::io::Result<bool> {
    let pid = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid process id"))?;
    match rustix::process::test_kill_process(pid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Ok(false),
        Err(rustix::io::Errno::SRCH) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// Poll `pid` until it exits, or `timeout` passes. `Ok(true)` means it exited.
pub(crate) fn wait_for_exit(pid: u32, timeout: Duration) -> std::io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if pid_exited(pid)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Spawn speedrund in its own process group so it outlives this process.
///
/// With `forced` false, `SPEEDRUN_NO_AUTOSTART` suppresses the spawn, as an
/// ordinary request does. An explicit `daemon restart` passes true: the user
/// asked for a daemon, so the variable does not veto it.
pub(crate) fn start_daemon(forced: bool) -> bool {
    if !forced && std::env::var_os("SPEEDRUN_NO_AUTOSTART").is_some() {
        return false;
    }
    let exe = daemon_executable();
    let dir = paths::data_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        report_io_error("create data directory", &dir, &e);
        return false;
    }
    let log_path = dir.join("speedrund.log");
    let log = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => Stdio::from(f),
        Err(e) => {
            report_io_error("open daemon log", &log_path, &e);
            Stdio::null()
        }
    };
    let mut cmd = Command::new(&exe);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    match cmd.spawn() {
        Ok(_) => true,
        Err(e) => {
            report_io_error("start", &exe, &e);
            false
        }
    }
}

/// The `speedrund` installed beside this `speedrun`, else the one on `PATH`.
fn daemon_executable() -> PathBuf {
    match std::env::current_exe() {
        Ok(cli) => {
            let sibling = cli.with_file_name("speedrund");
            if sibling.exists() {
                return sibling;
            }
        }
        Err(e) => report_io_error("locate", Path::new("speedrun"), &e),
    }
    PathBuf::from("speedrund")
}

#[cfg(test)]
mod tests {
    #[test]
    fn process_liveness_distinguishes_running_exited_and_invalid_pids() {
        assert!(!super::pid_exited(std::process::id()).unwrap());
        assert_eq!(
            super::pid_exited(0).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            super::pid_exited(u32::MAX).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(super::pid_exited(pid).unwrap());
    }

    #[test]
    fn rejected_diagnostics_are_private_paginated_and_key_independent() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let event_id = speedrun_core::new_id();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":event_id, "content":"private prompt", "token":"bearer-secret"}),
            credential: Credential::Agent,
        };
        let id = enqueue(&db, &entry, Some("owner")).unwrap();
        finish_rejected(
            &db,
            id,
            "owner",
            &CallError::Api {
                status: 422,
                message: "private server message".into(),
            },
            None,
        )
        .unwrap();
        db.execute(
            "INSERT INTO outbox(entry,state) VALUES (?1,'rejected')",
            ["{truncated bearer-secret"],
        )
        .unwrap();
        drop(db);
        fs::remove_file(dir.path().join("outbox.key")).unwrap();
        let first = rejected_page(dir.path(), 0, 1).unwrap();
        assert_eq!(first.entries[0].status, Some(422));
        assert_eq!(first.entries[0].reason, "http_rejection");
        assert_eq!(first.entries[0].request_kind, "prompt");
        assert_eq!(first.entries[0].event_id.as_deref(), Some(event_id.as_str()));
        assert!(first.entries[0].rejected_at_unix.is_some());
        let json = serde_json::to_string(&first).unwrap();
        for secret in [
            "private prompt",
            "bearer-secret",
            "private server message",
            "event-1",
        ] {
            assert!(!json.contains(secret));
        }
        let second = rejected_page(dir.path(), first.next_after.unwrap(), 1).unwrap();
        assert_eq!(second.entries[0].entry_format, "malformed_or_truncated");
        assert_eq!(second.entries[0].reason, "unknown");
    }

    #[test]
    fn paging_browses_current_rejections_not_a_rejection_stream() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id": speedrun_core::new_id(), "content": ""}),
            credential: Credential::Full,
        };
        let earlier = enqueue(&db, &entry, Some("a")).unwrap();
        let later = enqueue(&db, &entry, Some("b")).unwrap();
        let error = CallError::Api {
            status: 409,
            message: String::new(),
        };
        finish_rejected(&db, later, "b", &error, None).unwrap();
        assert_eq!(rejected_page(dir.path(), 0, 10).unwrap().entries[0].id, later);
        finish_rejected(&db, earlier, "a", &error, None).unwrap();
        assert!(rejected_page(dir.path(), later, 10).unwrap().entries.is_empty());
        assert_eq!(rejected_page(dir.path(), 0, 10).unwrap().entries.len(), 2);
    }

    #[test]
    fn metadata_only_rejection_is_inspectable_without_its_key() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id": speedrun_core::new_id(), "content":"", "metadata_only":{"length":42}}),
            credential: Credential::Full,
        };
        let id = enqueue(&db, &entry, Some("owner")).unwrap();
        finish_rejected(
            &db,
            id,
            "owner",
            &CallError::Api {
                status: 409,
                message: "secret".into(),
            },
            None,
        )
        .unwrap();
        drop(db);
        fs::remove_file(dir.path().join("outbox.key")).unwrap();
        let page = rejected_page(dir.path(), 0, 5).unwrap();
        assert_eq!(page.entries[0].suggested_action, "check_conflict");
        assert_eq!(page.entries[0].request_kind, "prompt");
        assert!(!serde_json::to_string(&page).unwrap().contains("secret"));
    }

    #[test]
    fn legacy_diagnostics_and_cursor_survive_migration_and_deletion() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE outbox (id INTEGER PRIMARY KEY, entry TEXT NOT NULL, state TEXT NOT NULL, owner TEXT, lease_until INTEGER, source TEXT UNIQUE); INSERT INTO outbox(id,entry,state) VALUES (9,'{\"path\":\"/legacy/rejected\",\"body\":{}}','rejected');").unwrap();
        drop(db);
        fs::write(dir.path().join("rejected.jsonl"), b"legacy secret\n").unwrap();
        let page = rejected_page(dir.path(), 0, 10).unwrap();
        assert_eq!(page.legacy_unimported, 1);
        assert_eq!(page.entries[0].reason, "unknown");
        let db = open_outbox_at(dir.path(), None).unwrap();
        db.execute("DELETE FROM outbox", []).unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"x","content":""}),
            credential: Credential::Full,
        };
        assert!(enqueue(&db, &entry, None).unwrap() > 9);
    }

    use super::*;

    fn sample_entry() -> SpoolEntry {
        SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"event-1"}),
            credential: Credential::Agent,
        }
    }

    #[test]
    fn encrypted_prompt_round_trips_and_rejects_tampering_or_missing_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.sqlite3");
        let db = open_outbox_path(&path).unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"event-1", "content":"private prompt"}),
            credential: Credential::Full,
        };
        let id = enqueue(&db, &entry, None).unwrap();
        let stored: String = db
            .query_row("SELECT entry FROM outbox WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert!(!stored.contains("private prompt"));
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(b"private prompt".len())
                .any(|w| w == b"private prompt")
        );
        let decoded = unprotect_entry(&db, serde_json::from_str(&stored).unwrap()).unwrap();
        assert_eq!(decoded.body["content"], "private prompt");
        let mut tampered: SpoolEntry = serde_json::from_str(&stored).unwrap();
        tampered.body["event_id"] = serde_json::json!("another-event");
        assert!(unprotect_entry(&db, tampered).is_err());
        let mut tampered: SpoolEntry = serde_json::from_str(&stored).unwrap();
        tampered.path = "/v1/other".into();
        assert!(unprotect_entry(&db, tampered).is_err());
        validate_existing_outbox(&path).unwrap();
        let mut corrupted: SpoolEntry = serde_json::from_str(&stored).unwrap();
        corrupted.body["content"]["outbox_ciphertext"] = serde_json::json!("00");
        db.execute(
            "UPDATE outbox SET entry=?1 WHERE id=?2",
            params![serde_json::to_string(&corrupted).unwrap(), id],
        )
        .unwrap();
        assert!(validate_existing_outbox(&path).is_err());
        db.execute("UPDATE outbox SET entry=?1 WHERE id=?2", params![stored, id])
            .unwrap();
        fs::remove_file(dir.path().join("outbox.key")).unwrap();
        assert!(validate_existing_outbox(&path).is_err());
        assert!(open_outbox_path(&path).is_err());
    }

    #[test]
    fn malformed_legacy_record_is_sealed_and_does_not_block_later_entries() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"later", "content":"later prompt"}),
            credential: Credential::Full,
        };
        let bad = b"unreadable secret \xff";
        let mut bytes = bad.to_vec();
        bytes.push(b'\n');
        bytes.extend_from_slice(serde_json::to_string(&entry).unwrap().as_bytes());
        fs::write(dir.path().join("pending.jsonl"), bytes).unwrap();
        import_legacy(&db, dir.path()).unwrap();
        let stored: String = db
            .query_row("SELECT entry FROM outbox WHERE state='rejected'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!stored.contains("unreadable secret"));
        let diagnostic: SpoolEntry = serde_json::from_str(&stored).unwrap();
        let source = diagnostic.body["source"].as_str().unwrap();
        assert_eq!(
            open_sealed(
                &read_outbox_key(&db).unwrap(),
                &diagnostic.body["sealed_legacy"],
                source.as_bytes()
            )
            .unwrap(),
            bad
        );
        assert_eq!(
            claim(&db, "owner").unwrap().unwrap().1.body["content"],
            "later prompt"
        );
    }

    #[test]
    fn legacy_jsonl_import_encrypts_pending_and_rejected_records() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_at(dir.path(), None).unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"legacy", "content":"pending secret"}),
            credential: Credential::Full,
        };
        fs::write(
            dir.path().join("pending.jsonl"),
            format!("{}\n", serde_json::to_string(&entry).unwrap()),
        )
        .unwrap();
        fs::write(dir.path().join("rejected.jsonl"), b"old rejected secret\n").unwrap();
        import_legacy(&db, dir.path()).unwrap();
        assert!(!dir.path().join("pending.jsonl").exists());
        assert!(!dir.path().join("rejected.jsonl").exists());
        let entries: Vec<String> = db
            .prepare("SELECT entry FROM outbox ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            entries
                .iter()
                .all(|e| !e.contains("pending secret") && !e.contains("old rejected secret"))
        );
        assert_eq!(
            claim(&db, "owner").unwrap().unwrap().1.body["content"],
            "pending secret"
        );
    }

    #[test]
    fn recovery_validation_rejects_unrelated_and_uninitialized_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.sqlite3");
        let db = Connection::open(&path).unwrap();
        assert!(validate_existing_outbox(&path).is_err());
        db.execute_batch("CREATE TABLE outbox (id INTEGER PRIMARY KEY, entry TEXT NOT NULL, state TEXT NOT NULL, owner TEXT, lease_until INTEGER, source TEXT UNIQUE)").unwrap();
        assert!(validate_existing_outbox(&path).is_ok());
        db.execute_batch("CREATE TABLE outbox_crypto (id INTEGER PRIMARY KEY, verifier TEXT NOT NULL)")
            .unwrap();
        assert!(validate_existing_outbox(&path).is_err());
    }

    #[test]
    fn plaintext_legacy_sqlite_rows_are_migrated_before_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.sqlite3");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE outbox (id INTEGER PRIMARY KEY, entry TEXT NOT NULL, state TEXT NOT NULL, owner TEXT, lease_until INTEGER, source TEXT UNIQUE)").unwrap();
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"legacy", "content":"legacy secret"}),
            credential: Credential::Full,
        };
        db.execute(
            "INSERT INTO outbox(entry,state) VALUES (?1,'pending')",
            [serde_json::to_string(&entry).unwrap()],
        )
        .unwrap();
        drop(db);
        let db = open_outbox_path(&path).unwrap();
        let stored: String = db
            .query_row("SELECT entry FROM outbox", [], |r| r.get(0))
            .unwrap();
        assert!(!stored.contains("legacy secret"));
        assert_eq!(
            claim(&db, "owner").unwrap().unwrap().1.body["content"],
            "legacy secret"
        );
        assert!(
            !fs::read(&path)
                .unwrap()
                .windows(b"legacy secret".len())
                .any(|w| w == b"legacy secret")
        );
    }

    #[test]
    fn metadata_only_prompt_never_reaches_the_outbox_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_path(&dir.path().join("test.sqlite")).unwrap();
        let raw = "private words that must not be persisted";
        let (content, metadata) =
            crate::ops::prompt_capture(raw, crate::preferences::CaptureMode::MetadataOnly);
        let entry = SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"content": content, "metadata_only": metadata}),
            credential: Credential::Full,
        };
        let id = enqueue(&db, &entry, None).unwrap();
        let stored: String = db
            .query_row("SELECT entry FROM outbox WHERE id=?1", [id], |row| row.get(0))
            .unwrap();
        assert!(!stored.contains(raw));
        let value: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(value["body"]["content"], "");
        assert_eq!(value["body"]["metadata_only"]["length"], raw.chars().count());
    }

    #[test]
    fn failed_acknowledgement_keeps_the_original_and_expired_lease_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_path(&dir.path().join("test.sqlite")).unwrap();
        let id = enqueue(&db, &sample_entry(), Some("crashed")).unwrap();
        db.execute_batch(
            "CREATE TRIGGER fail_ack BEFORE DELETE ON outbox BEGIN SELECT RAISE(ABORT, 'disk failure'); END;",
        )
        .unwrap();
        assert!(finish(&db, id, "crashed", false).is_err());
        db.execute_batch("DROP TRIGGER fail_ack; UPDATE outbox SET lease_until=0;")
            .unwrap();
        let (recovered_id, recovered) = claim(&db, "recovery").unwrap().unwrap();
        assert_eq!(id, recovered_id);
        assert_eq!(recovered.body["event_id"], "event-1");
        assert!(matches!(recovered.credential, Credential::Agent));
        finish(&db, id, "recovery", false).unwrap();
        assert!(claim(&db, "other").unwrap().is_none());
    }

    #[test]
    fn concurrent_flushers_claim_distinct_rows() {
        use std::sync::{
            Barrier,
            atomic::{AtomicBool, Ordering},
        };

        const WORKERS: usize = 4;
        const ROUNDS: usize = 5;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let db = open_outbox_path(&path).unwrap();
        for _ in 0..(WORKERS * ROUNDS) {
            enqueue(&db, &sample_entry(), None).unwrap();
        }
        let connections = (0..WORKERS)
            .map(|_| open_outbox_path(&path).unwrap())
            .collect::<Vec<_>>();
        let claims = std::sync::Mutex::new(Vec::new());
        let errors = std::sync::Mutex::new(Vec::new());
        let failed = AtomicBool::new(false);
        let round = Barrier::new(WORKERS);
        // SQLite may return BUSY after the configured bounded wait. A real
        // flusher reports that transient failure and a later flush can retry;
        // exercise that contract while still requiring distinct claims.
        fn retry_busy<T>(mut action: impl FnMut() -> anyhow::Result<T>) -> anyhow::Result<T> {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match action() {
                    Ok(value) => return Ok(value),
                    Err(error)
                        if matches!(
                            error.downcast_ref::<rusqlite::Error>(),
                            Some(rusqlite::Error::SqliteFailure(inner, _))
                                if matches!(inner.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                        ) && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        std::thread::scope(|scope| {
            for (n, db) in connections.into_iter().enumerate() {
                let claims = &claims;
                let errors = &errors;
                let failed = &failed;
                let round = &round;
                scope.spawn(move || {
                    let owner = format!("worker-{n}");
                    // Every worker competes once per round. No worker can
                    // monopolize the database and make scheduler fairness a
                    // hidden precondition of the duplicate-claim assertion.
                    for _ in 0..ROUNDS {
                        round.wait();
                        match retry_busy(|| claim(&db, &owner)) {
                            Ok(Some((id, _))) => claims.lock().unwrap().push((id, owner.clone())),
                            Ok(None) => {
                                errors
                                    .lock()
                                    .unwrap()
                                    .push("outbox became empty early".to_owned());
                                failed.store(true, Ordering::SeqCst);
                            }
                            Err(error) => {
                                errors
                                    .lock()
                                    .unwrap()
                                    .push(format!("outbox claim failed: {error:#}"));
                                failed.store(true, Ordering::SeqCst);
                            }
                        }
                        round.wait();
                        if failed.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                });
            }
        });
        let errors = errors.into_inner().unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        let claims = claims.into_inner().unwrap();
        let mut ids = claims.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        assert_eq!(ids.len(), WORKERS * ROUNDS);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), WORKERS * ROUNDS);
        for (id, owner) in claims {
            finish(&db, id, &owner, false).unwrap();
        }
        assert!(claim(&db, "after").unwrap().is_none());
    }

    #[test]
    fn initialized_outbox_opens_while_another_writer_holds_a_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let writer = open_outbox_path(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let deadline = Instant::now() + Duration::from_millis(100);
        let reader = open_outbox_path_with_deadline(&path, Some(deadline)).unwrap();
        drop(reader);
        writer.execute_batch("ROLLBACK;").unwrap();
    }

    #[test]
    fn failed_rejection_does_not_discard_the_leased_entry() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_outbox_path(&dir.path().join("test.sqlite")).unwrap();
        let id = enqueue(&db, &sample_entry(), Some("worker")).unwrap();
        db.execute_batch("CREATE TRIGGER fail_reject BEFORE UPDATE ON outbox WHEN NEW.state='rejected' BEGIN SELECT RAISE(ABORT, 'disk failure'); END;").unwrap();
        assert!(finish(&db, id, "worker", true).is_err());
        let state: String = db
            .query_row("SELECT state FROM outbox WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "leased");
    }

    #[test]
    fn bounded_flush_skips_a_locked_legacy_import() {
        let dir = tempfile::tempdir().unwrap();
        // Initialize before the timed part so slow durable filesystem setup is
        // not confused with waiting for the unrelated legacy import lock.
        drop(open_outbox_at(dir.path(), None).unwrap());
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.path().join("import.lock"))
            .unwrap();
        lock.lock().unwrap();
        let entry = serde_json::to_string(&sample_entry()).unwrap();
        fs::write(dir.path().join("pending.jsonl"), format!("{entry}\n")).unwrap();
        let path = dir.path().to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<bool> {
                let db = prepare_flush(&path, Some(Instant::now() + Duration::from_secs(15)))?
                    .ok_or_else(|| anyhow::anyhow!("flush deadline elapsed before opening"))?;
                Ok(claim(&db, "hook")?.is_none())
            })();
            let _ = tx.send(result);
        });
        let completed_while_locked = rx.recv_timeout(Duration::from_secs(5));
        // Release before joining so a regression waiting on this lock can exit.
        drop(lock);
        worker.join().unwrap();
        assert!(completed_while_locked.unwrap().unwrap());
        assert!(dir.path().join("pending.jsonl").exists());
        let db = prepare_flush(dir.path(), None).unwrap().unwrap();
        assert!(claim(&db, "explicit").unwrap().is_some());
        assert!(!dir.path().join("pending.jsonl").exists());
    }

    #[test]
    fn sqlite_open_and_acknowledgement_respect_the_flush_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending.sqlite3");
        let db = open_outbox_path(&path).unwrap();
        let id = enqueue(&db, &sample_entry(), Some("worker")).unwrap();
        db.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let start = Instant::now();
        assert!(prepare_flush(dir.path(), Some(start + Duration::from_millis(100))).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        db.execute_batch("ROLLBACK;").unwrap();

        let reader = open_outbox_path(&path).unwrap();
        reader.execute_batch("BEGIN;").unwrap();
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
            .unwrap();
        let start = Instant::now();
        assert!(
            finish_with_deadline(&db, id, "worker", false, Some(start + Duration::from_millis(100))).is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        reader.execute_batch("ROLLBACK;").unwrap();
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM outbox WHERE id=?1", [id], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "failed acknowledgement must keep the event");
    }

    /// M7.d: writers refused at the same moment, with the log trimming under
    /// them. Trimming drops the oldest lines, so each writer's surviving lines
    /// must be the newest of its own, with no gap: a gap, or a missing newest
    /// line, is a line one trim wrote over after another writer appended it.
    /// The race is narrow, so the scenario runs in many fresh logs.
    #[test]
    fn trimming_the_rejected_log_loses_no_line_under_concurrent_refusals() {
        const ROUNDS: usize = 40;
        const WRITERS: usize = 8;
        const LINES: usize = 300;
        for round in 0..ROUNDS {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("rejected.jsonl");
            std::thread::scope(|scope| {
                for writer in 0..WRITERS {
                    let file = file.clone();
                    scope.spawn(move || {
                        let log = RejectedLog::with_cap(file, 4 * 1024);
                        for n in 0..LINES {
                            let line = format!("{writer:02}:{n:04}:{}", "x".repeat(40));
                            log.try_append(line.as_bytes()).unwrap();
                        }
                    });
                }
            });
            let text = fs::read_to_string(&file).unwrap();
            let mut kept: Vec<Vec<usize>> = vec![Vec::new(); WRITERS];
            for line in text.lines() {
                let mut parts = line.split(':');
                let writer: usize = parts.next().unwrap().parse().unwrap();
                let n: usize = parts.next().unwrap().parse().unwrap();
                kept[writer].push(n);
            }
            assert!(
                kept.iter().any(|kept| !kept.is_empty()),
                "round {round}: the log is empty"
            );
            for (writer, kept) in kept.iter().enumerate() {
                // A writer that finished early may be trimmed away entirely.
                if kept.is_empty() {
                    continue;
                }
                assert_eq!(
                    kept.last(),
                    Some(&(LINES - 1)),
                    "round {round}: writer {writer} lost its newest line"
                );
                for pair in kept.windows(2) {
                    assert_eq!(
                        pair[1],
                        pair[0] + 1,
                        "round {round}: writer {writer} lost a line after {}",
                        pair[0]
                    );
                }
            }
        }
    }
}
