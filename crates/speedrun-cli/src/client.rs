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
#[derive(Serialize, Deserialize)]
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
        if proof == expected.as_str() {
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

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.ready()?;
        self.send("GET", path, None, Delivery::Fresh, self.credential)
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
                if let Err(storage) = finish(&db, id, &owner, true) {
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
        let db = open_outbox().map_err(storage_error)?;
        import_legacy(&db).map_err(storage_error)?;
        let owner = speedrun_core::new_id();
        let mut delivered = 0;
        loop {
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
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
                    finish(&db, id, &owner, false).map_err(storage_error)?;
                    delivered += 1;
                }
                Err(error) if error.should_spool() => {
                    release(&db, id, &owner).map_err(storage_error)?;
                    break;
                }
                Err(_) => finish(&db, id, &owner, true).map_err(storage_error)?,
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

fn open_outbox() -> anyhow::Result<Connection> {
    let dir = spool_dir();
    fs::create_dir_all(&dir)?;
    let path = dir.join("pending.sqlite3");
    // Create with private permissions before SQLite opens the file.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    open_outbox_path(&path)
}

fn open_outbox_path(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.execute_batch(
        "PRAGMA synchronous=FULL; PRAGMA auto_vacuum=FULL; PRAGMA secure_delete=ON;
        CREATE TABLE IF NOT EXISTS outbox (
            id INTEGER PRIMARY KEY, entry TEXT NOT NULL, state TEXT NOT NULL,
            owner TEXT, lease_until INTEGER, source TEXT UNIQUE
        );",
    )?;
    Ok(db)
}

fn enqueue(db: &Connection, entry: &SpoolEntry, owner: Option<&str>) -> anyhow::Result<i64> {
    let encoded = serde_json::to_string(entry)?;
    db.execute(
        "INSERT INTO outbox(entry,state,owner,lease_until) VALUES (?1,?2,?3,?4)",
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
    row.map(|(id, entry)| Ok((id, serde_json::from_str(&entry)?)))
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
    let tx = db.unchecked_transaction()?;
    if rejected {
        let changed = tx.execute(
            "UPDATE outbox SET state='rejected',entry=substr(CAST(entry AS BLOB),1,4194304),owner=NULL,lease_until=NULL WHERE id=?1 AND owner=?2 AND state='leased'",
            params![id, owner],
        )?;
        anyhow::ensure!(changed == 1, "outbox lease was lost");
        // Keep the newest bounded diagnostic records, including at most one oversized entry.
        tx.execute("DELETE FROM outbox WHERE state='rejected' AND id NOT IN (SELECT id FROM (SELECT id, SUM(length(CAST(entry AS BLOB))) OVER (ORDER BY id DESC) AS total, ROW_NUMBER() OVER (ORDER BY id DESC) AS n FROM outbox WHERE state='rejected') WHERE total<=?1 OR n=1)", [REJECTED_LIMIT])?;
    } else {
        let changed = tx.execute(
            "DELETE FROM outbox WHERE id=?1 AND owner=?2 AND state='leased'",
            params![id, owner],
        )?;
        anyhow::ensure!(changed == 1, "outbox lease was lost");
    }
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
    report_storage_error(&error);
    CallError::Storage(error.to_string())
}

/// Import old files transactionally. A source/line identity prevents duplicates
/// if a process dies after commit but before removing the source file.
fn import_legacy(db: &Connection) -> anyhow::Result<()> {
    let dir = spool_dir();
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join("import.lock"))?;
    lock.lock()?;
    for item in fs::read_dir(&dir)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().into_owned();
        if name != "pending.jsonl" && !name.starts_with("flushing-") {
            continue;
        }
        let bytes = fs::read(item.path())?;
        let tx = db.unchecked_transaction()?;
        for (number, line) in bytes.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let source = format!("{name}:{number}:{}", speedrun_core::hash::sha256_hex(line));
            if let Ok(entry) = serde_json::from_slice::<SpoolEntry>(line) {
                tx.execute(
                    "INSERT OR IGNORE INTO outbox(entry,state,source) VALUES (?1,'pending',?2)",
                    params![serde_json::to_string(&entry)?, source],
                )?;
            } else {
                // Legacy malformed records remain inspectable. The source is
                // retained if writing this diagnostic fails.
                RejectedLog::new(dir.join("rejected.jsonl")).try_append(line)?;
            }
        }
        tx.commit()?;
        fs::remove_file(item.path())?;
    }
    Ok(())
}

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

/// Newest refused entries are kept for inspection, but the log must not grow
/// without bound no matter how many the daemon refuses.
const REJECTED_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// A capped log of refused spool entries. `append` is the only way to write
/// to it, and it enforces the cap, so callers cannot leave the file unbounded.
struct RejectedLog {
    file: PathBuf,
    max_bytes: u64,
}

impl RejectedLog {
    fn new(file: PathBuf) -> Self {
        Self::with_cap(file, REJECTED_MAX_BYTES)
    }

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

/// Whether `pid` has exited, by `kill -0` on the `kill` binary. Not `ps`,
/// which the check sandbox cannot execute because it is setuid.
pub(crate) fn pid_exited(pid: u32) -> std::io::Result<bool> {
    let pid = pid.to_string();
    let status = Command::new("kill")
        .args(["-0", pid.as_str()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(!status.success())
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
    use super::*;

    fn sample_entry() -> SpoolEntry {
        SpoolEntry {
            path: "/v1/prompts".into(),
            body: serde_json::json!({"event_id":"event-1"}),
            credential: Credential::Agent,
        }
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        let db = open_outbox_path(&path).unwrap();
        for _ in 0..100 {
            enqueue(&db, &sample_entry(), None).unwrap();
        }
        let ids = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for n in 0..4 {
                let path = &path;
                let ids = &ids;
                scope.spawn(move || {
                    let db = open_outbox_path(path).unwrap();
                    let owner = format!("worker-{n}");
                    while let Some((id, _)) = claim(&db, &owner).unwrap() {
                        ids.lock().unwrap().push(id);
                        finish(&db, id, &owner, false).unwrap();
                    }
                });
            }
        });
        let mut ids = ids.into_inner().unwrap();
        assert_eq!(ids.len(), 100);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 100);
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
