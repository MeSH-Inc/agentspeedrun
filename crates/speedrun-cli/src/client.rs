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
use std::time::{Duration, Instant, SystemTime};

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
    /// The daemon answered with an error status.
    Api { status: u16, message: String },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) => write!(f, "speedrund unreachable: {m}"),
            Self::WrongDataDir { addr } => {
                write!(f, "daemon answers for another data directory ({addr})")
            }
            Self::Api { status, message } => write!(f, "speedrund error {status}: {message}"),
        }
    }
}

impl std::error::Error for CallError {}

impl CallError {
    /// Whether this failure is worth keeping in the spool: the daemon is
    /// unreachable (a timeout, no listener, or no proof it holds this data
    /// dir's token), refused the token even after the client re-read it
    /// (401), or failed on its own side (5xx). Any other 4xx is the client's
    /// mistake and is reported as it always was.
    fn should_spool(&self) -> bool {
        match self {
            Self::Unreachable(_) | Self::WrongDataDir { .. } => true,
            Self::Api { status, .. } => *status == 401 || *status >= 500,
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
    Ok(SpoolStats {
        spooled,
        rejected: count_lines(&dir.join("rejected.jsonl"))?,
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
            Ok(r) => r.into_json().map_err(|e| CallError::Api {
                status: 200,
                message: format!("unreadable response: {e}"),
            }),
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
    ) -> Result<Option<T>, CallError> {
        let credential = self.credential;
        let outcome = match self.ready() {
            Ok(()) => {
                let outcome = self.send("POST", path, Some(body), Delivery::Fresh, credential);
                // The fresh write has been attempted; only now touch the backlog.
                self.flush_spool(Some(Instant::now() + FLUSH_BUDGET));
                outcome
            }
            // Unproven daemon: keep the event; a later invocation that can
            // prove the token will deliver it with the backlog.
            Err(e) if e.should_spool() => {
                spool(path, body, credential);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        match outcome {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.should_spool() => {
                spool(path, body, credential);
                Ok(None)
            }
            Err(e) => Err(e),
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

    /// Deliver spooled requests (call `ready` first to start the daemon). Claims the spool by renaming it, so
    /// concurrent hook processes never deliver the same file twice.
    ///
    /// `deadline` bounds the wall clock spent here. Once it passes, the entry
    /// in hand and every entry after it go back to the pending spool in their
    /// original order, for a later flush. `None` means no bound.
    pub fn flush_spool(&self, deadline: Option<Instant>) -> usize {
        let dir = spool_dir();
        let live = dir.join("pending.jsonl");
        let claimed = dir.join(format!("flushing-{}.jsonl", speedrun_core::new_id()));
        let mut files = Vec::new();
        match fs::rename(&live, &claimed) {
            Ok(()) => files.push(claimed),
            // Nothing is pending.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => report_io_error("claim spool file", &live, &e),
        }
        // Pick up files abandoned by a process that died mid-flush.
        match fs::read_dir(&dir) {
            Ok(entries) => {
                let stale = SystemTime::now() - Duration::from_secs(120);
                for e in entries.flatten() {
                    let p = e.path();
                    let flushing = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("flushing-"));
                    let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < stale);
                    if flushing && old && !files.contains(&p) {
                        files.push(p);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => report_io_error("list spool directory", &dir, &e),
        }
        let rejected = RejectedLog::new(dir.join("rejected.jsonl"));
        let mut delivered = 0;
        let mut out_of_time = false;
        for file in files {
            let f = match fs::File::open(&file) {
                Ok(f) => f,
                // Another flush claimed it first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    report_io_error("read spool file", &file, &e);
                    continue;
                }
            };
            let mut lines = BufReader::new(f).split(b'\n');
            let mut keep_file = false;
            while let Some(read) = lines.next() {
                let bytes = match read {
                    Ok(bytes) => bytes,
                    // Keep the file: a later flush retries it once it is stale, and every
                    // entry carries an event_id, so the ones already delivered are not doubled.
                    Err(e) => {
                        report_io_error("read spool file", &file, &e);
                        keep_file = true;
                        break;
                    }
                };
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    // Out of time: put this entry and the rest back, in order, so a
                    // later flush reaches them instead of spending a hook's budget.
                    out_of_time = true;
                    append_line(&live, &bytes);
                    for rest in lines.by_ref() {
                        match rest {
                            Ok(rest) => append_line(&live, &rest),
                            Err(e) => {
                                report_io_error("read spool file", &file, &e);
                                keep_file = true;
                                break;
                            }
                        }
                    }
                    break;
                }
                // A line that is not text or not an entry can never be delivered; keep it
                // for inspection and go on with the rest of the file.
                let entry = match serde_json::from_slice::<SpoolEntry>(&bytes) {
                    Ok(entry) => entry,
                    Err(e) => {
                        report_unreadable_entry(&file, &e);
                        rejected.append(&bytes);
                        continue;
                    }
                };
                match self.send::<Value>(
                    "POST",
                    &entry.path,
                    Some(&entry.body),
                    Delivery::Replay { deadline },
                    entry.credential,
                ) {
                    Ok(_) => delivered += 1,
                    Err(e) => {
                        if e.should_spool() {
                            // Still ours to deliver: put it back and break so a later
                            // flush reaches it. Already-delivered entries are not
                            // doubled, because every one carries an event_id.
                            append_line(&live, &bytes);
                            for rest in lines.by_ref() {
                                match rest {
                                    Ok(rest) => append_line(&live, &rest),
                                    Err(e) => {
                                        report_io_error("read spool file", &file, &e);
                                        keep_file = true;
                                        break;
                                    }
                                }
                            }
                            break;
                        }
                        // The daemon refused it; keep it for inspection rather than retrying forever.
                        rejected.append(&bytes);
                    }
                }
            }
            if !keep_file && let Err(e) = fs::remove_file(&file) {
                report_io_error("remove spool file", &file, &e);
            }
            if out_of_time {
                break;
            }
        }
        delivered
    }
}

fn spool(path: &str, body: &Value, credential: Credential) {
    let file = spool_dir().join("pending.jsonl");
    let entry = SpoolEntry {
        path: path.to_string(),
        body: body.clone(),
        credential,
    };
    match serde_json::to_string(&entry) {
        Ok(line) => append_line(&file, line.as_bytes()),
        // The event is lost, so it counts as a failed spool write.
        Err(e) => report_spool_write_error("encode spool entry", &file, &std::io::Error::other(e)),
    }
}

fn append_line(file: &Path, line: &[u8]) {
    if let Err(e) = append_line_result(file, line) {
        report_spool_write_error("write spool file", file, &e);
    }
}

/// One write per line with O_APPEND keeps concurrent writers from interleaving.
fn append_line_result(file: &Path, line: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(file)?;
    let mut record = Vec::with_capacity(line.len() + 1);
    record.extend_from_slice(line);
    record.push(b'\n');
    f.write_all(&record)
}

fn report_unreadable_entry(file: &Path, e: &serde_json::Error) {
    if std::env::var_os("SPEEDRUN_DEBUG").is_some() {
        eprintln!("speedrun: refused an unreadable entry in {}: {e}", file.display());
    }
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

    fn append(&self, line: &[u8]) {
        if let Err(e) = self.try_append(line) {
            report_spool_write_error("write refused-event log", &self.file, &e);
        }
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
        if current.saturating_add(line.len() as u64 + 1) > self.max_bytes {
            self.trim_to_newest()?;
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
