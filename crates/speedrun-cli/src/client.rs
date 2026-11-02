//! HTTP client for speedrund, with an on-disk spool so writes made while
//! the daemon is down are delivered later instead of lost. Every write
//! carries an event_id, so replaying the spool is safe.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use speedrun_core::{api::ErrorBody, live::LiveSnapshot, paths};

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

#[derive(Serialize, Deserialize)]
struct SpoolEntry {
    path: String,
    body: Value,
}

pub struct Client {
    base: String,
    agent: ureq::Agent,
}

fn spool_dir() -> PathBuf {
    paths::data_dir().join("spool")
}

/// Header the daemon reads to exempt a spooled write from its last-event
/// clock bound. Only `flush_spool` marks a delivery this way.
const REPLAY_HEADER: &str = "X-Speedrun-Replay";

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
            base: format!("http://{}", paths::daemon_addr()),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_millis(500))
                .timeout(timeout)
                .build(),
        }
    }

    fn token() -> Result<String, CallError> {
        fs::read_to_string(paths::token_path())
            .map(|t| t.trim().to_string())
            .map_err(|_| CallError::Unreachable("no token yet; has speedrund ever run?".into()))
    }

    fn send<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        from_spool: bool,
    ) -> Result<T, CallError> {
        let mut req = self
            .agent
            .request(method, &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", Self::token()?));
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
            Err(ureq::Error::Status(status, r)) => Err(CallError::Api {
                status,
                message: r
                    .into_json::<ErrorBody>()
                    .map(|b| b.error)
                    .unwrap_or_else(|_| "unknown error".into()),
            }),
            Err(ureq::Error::Transport(t)) => Err(CallError::Unreachable(t.to_string())),
        }
    }

    /// Make sure the daemon is up (starting it if needed), then deliver any
    /// spooled events first so they keep their order relative to new ones.
    pub fn ready(&self) -> Result<(), CallError> {
        self.with_autostart(|| self.send::<Value>("GET", "/v1/health", None, false))?;
        self.flush_spool();
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.ready()?;
        self.send("GET", path, None, false)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, CallError> {
        self.ready()?;
        self.send("POST", path, Some(body), false)
    }

    /// POST that must not be lost: if the daemon is unreachable the request
    /// is spooled and `Ok(None)` is returned.
    pub fn post_durable<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<Option<T>, CallError> {
        match self.post(path, body) {
            Ok(v) => Ok(Some(v)),
            Err(CallError::Unreachable(_)) => {
                spool(path, body);
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
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
            .get(&format!("{}/v1/live/stream?since_hours={since_hours}", self.base))
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
    pub fn flush_spool(&self) -> usize {
        let dir = spool_dir();
        let live = dir.join("pending.jsonl");
        let claimed = dir.join(format!("flushing-{}.jsonl", speedrun_core::new_id()));
        let mut files = Vec::new();
        if fs::rename(&live, &claimed).is_ok() {
            files.push(claimed);
        }
        // Pick up files abandoned by a process that died mid-flush.
        if let Ok(entries) = fs::read_dir(&dir) {
            let stale = SystemTime::now() - Duration::from_secs(120);
            for e in entries.flatten() {
                let p = e.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m < stale);
                if name.starts_with("flushing-") && old && !files.contains(&p) {
                    files.push(p);
                }
            }
        }
        let rejected = RejectedLog::new(dir.join("rejected.jsonl"));
        let mut delivered = 0;
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
                    Err(CallError::Unreachable(_)) => {
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
                    Err(CallError::Api { .. }) => rejected.append(&bytes),
                }
            }
            if !keep_file && let Err(e) = fs::remove_file(&file) {
                report_io_error("remove spool file", &file, &e);
            }
        }
        delivered
    }
}

fn spool(path: &str, body: &Value) {
    let line = serde_json::to_string(&SpoolEntry {
        path: path.to_string(),
        body: body.clone(),
    })
    .expect("spool entry serializes");
    append_line(&spool_dir().join("pending.jsonl"), line.as_bytes());
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
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("speedrund")))
        .filter(|p| p.exists());
    let exe = sibling.unwrap_or_else(|| PathBuf::from("speedrund"));
    let dir = paths::data_dir();
    let _ = fs::create_dir_all(&dir);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("speedrund.log"));
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).process_group(0);
    match log {
        Ok(f) => cmd.stderr(f),
        Err(_) => cmd.stderr(Stdio::null()),
    };
    cmd.spawn().is_ok()
}
