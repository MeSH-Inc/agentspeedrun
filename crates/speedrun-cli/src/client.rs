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
    /// The daemon answered with an error status.
    Api { status: u16, message: String },
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(m) => write!(f, "speedrund unreachable: {m}"),
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
            Self::Unreachable(_) => true,
            Self::Api { status, .. } => *status == 401 || *status >= 500,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SpoolEntry {
    path: String,
    body: Value,
}

pub struct Client {
    agent: ureq::Agent,
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
fn resolve_addr() -> String {
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

/// Header the daemon reads to exempt a spooled write from its last-event
/// clock bound. Only `flush_spool` marks a delivery this way.
const REPLAY_HEADER: &str = "X-Speedrun-Replay";

/// How long a durable write may spend draining the backlog after its own
/// event has been attempted. Hooks must return to their host quickly, so an
/// entry this does not reach stays spooled for a later flush.
const FLUSH_BUDGET: Duration = Duration::from_secs(3);

impl Client {
    pub fn new() -> Self {
        Self::with_timeout(Duration::from_secs(5))
    }

    /// For milestone claims: the daemon checks evidence before answering,
    /// which can take several network round trips.
    pub fn patient() -> Self {
        Self::with_timeout(Duration::from_secs(120))
    }

    fn with_timeout(timeout: Duration) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_millis(500))
                .timeout(timeout)
                .build(),
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

    fn token() -> Result<String, CallError> {
        fs::read_to_string(paths::token_path())
            .map(|t| t.trim().to_string())
            .map_err(|_| CallError::Unreachable("no token yet; has speedrund ever run?".into()))
    }

    /// Send one request, re-reading the token and retrying once on 401: a
    /// daemon that restarted wrote a fresh token, and the first read may
    /// predate it.
    fn send<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        from_spool: bool,
    ) -> Result<T, CallError> {
        let mut retried = false;
        loop {
            let token = Self::token()?;
            match self.attempt(method, path, body, from_spool, &token) {
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
        from_spool: bool,
        token: &str,
    ) -> Result<T, CallError> {
        let mut req = self
            .agent
            .request(method, &format!("{}{path}", self.verified_base()?))
            .set("Authorization", &format!("Bearer {token}"));
        if from_spool {
            req = req.set(REPLAY_HEADER, "1");
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
    fn verify_health(&self) -> Result<(), CallError> {
        let base = format!("http://{}", resolve_addr());
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
        let Some(proof) = health.proof else {
            return Err(CallError::Unreachable(format!(
                "daemon at {base} did not prove the token"
            )));
        };
        if proof == speedrun_core::digest::token_proof(&Self::token()?, &nonce) {
            *self.verified.borrow_mut() = Some(base);
            Ok(())
        } else {
            Err(CallError::Unreachable(format!(
                "daemon answers for another data directory ({base})"
            )))
        }
    }

    /// Make sure the daemon is up and has proved this data dir's token
    /// (starting it if needed). Delivery is deliberately separate: a durable
    /// write sends its own event before the backlog, and `flush_spool` owns
    /// how long that backlog may take.
    pub fn ready(&self) -> Result<(), CallError> {
        self.with_autostart(|| self.verify_health())
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.ready()?;
        self.send("GET", path, None, false)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, CallError> {
        self.ready()?;
        self.send("POST", path, Some(body), false)
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
        let outcome = match self.ready() {
            Ok(()) => {
                let outcome = self.send("POST", path, Some(body), false);
                // The fresh write has been attempted; only now touch the backlog.
                self.flush_spool(Some(Instant::now() + FLUSH_BUDGET));
                outcome
            }
            // Unproven daemon: keep the event; a later invocation that can
            // prove the token will deliver it with the backlog.
            Err(e) if e.should_spool() => {
                spool(path, body);
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        match outcome {
            Ok(value) => Ok(Some(value)),
            Err(e) if e.should_spool() => {
                spool(path, body);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// The daemon `ready` verified.
    pub fn base_url(&self) -> Result<String, CallError> {
        self.verified_base()
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
            Err(CallError::Unreachable(_)) if start_daemon() => {
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
                match self.send::<Value>("POST", &entry.path, Some(&entry.body), true) {
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

fn spool(path: &str, body: &Value) {
    let file = spool_dir().join("pending.jsonl");
    let entry = SpoolEntry {
        path: path.to_string(),
        body: body.clone(),
    };
    match serde_json::to_string(&entry) {
        Ok(line) => append_line(&file, line.as_bytes()),
        Err(e) => report_io_error("write spool file", &file, &std::io::Error::other(e)),
    }
}

fn append_line(file: &Path, line: &[u8]) {
    if let Err(e) = append_line_result(file, line) {
        report_io_error("write spool file", file, &e);
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

/// Newest refused entries are kept for inspection, but the log must not grow
/// without bound no matter how many the daemon refuses.
const REJECTED_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// A capped log of refused spool entries. `append` is the only way to write
/// to it, and it enforces the cap, so callers cannot leave the file unbounded.
struct RejectedLog {
    file: PathBuf,
}

impl RejectedLog {
    fn new(file: PathBuf) -> Self {
        Self { file }
    }

    fn append(&self, line: &[u8]) {
        if let Err(e) = self.try_append(line) {
            report_io_error("write refused-event log", &self.file, &e);
        }
    }

    fn try_append(&self, line: &[u8]) -> std::io::Result<()> {
        let current = match fs::metadata(&self.file) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        if current.saturating_add(line.len() as u64 + 1) > REJECTED_MAX_BYTES {
            self.trim_to_newest()?;
        }
        append_line_result(&self.file, line)
    }

    /// Keep the newest entries that fit in half the cap; the rest of the
    /// headroom amortizes trimming over several appends.
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
        let keep = REJECTED_MAX_BYTES / 2;
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
        fs::write(&self.file, out)
    }
}

/// Spawn speedrund in its own process group so it outlives this process.
fn start_daemon() -> bool {
    if std::env::var_os("SPEEDRUN_NO_AUTOSTART").is_some() {
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
