//! HTTP client for speedrund, with an on-disk spool so writes made while
//! the daemon is down are delivered later instead of lost. Every write
//! carries an event_id, so replaying the spool is safe.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use speedrun_core::{api::ErrorBody, paths};

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

impl Client {
    pub fn new() -> Self {
        Self {
            base: format!("http://{}", paths::daemon_addr()),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_millis(500))
                .timeout(Duration::from_secs(5))
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
    ) -> Result<T, CallError> {
        let req = self
            .agent
            .request(method, &format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", Self::token()?));
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
    fn ready(&self) -> Result<(), CallError> {
        self.with_autostart(|| self.send::<Value>("GET", "/v1/health", None))?;
        self.flush_spool();
        Ok(())
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, CallError> {
        self.ready()?;
        self.send("GET", path, None)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T, CallError> {
        self.ready()?;
        self.send("POST", path, Some(body))
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
        let mut delivered = 0;
        for file in files {
            let Ok(f) = fs::File::open(&file) else { continue };
            let mut lines = BufReader::new(f).lines().map_while(Result::ok);
            for line in lines.by_ref() {
                let Ok(entry) = serde_json::from_str::<SpoolEntry>(&line) else {
                    continue;
                };
                match self.send::<Value>("POST", &entry.path, Some(&entry.body)) {
                    Ok(_) => delivered += 1,
                    Err(CallError::Unreachable(_)) => {
                        append_line(&live, &line);
                        for rest in lines.by_ref() {
                            append_line(&live, &rest);
                        }
                        break;
                    }
                    // The daemon refused it; keep it for inspection rather than retrying forever.
                    Err(CallError::Api { .. }) => append_line(&dir.join("rejected.jsonl"), &line),
                }
            }
            let _ = fs::remove_file(&file);
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
    append_line(&spool_dir().join("pending.jsonl"), &line);
}

fn append_line(file: &PathBuf, line: &str) {
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    // One write per line with O_APPEND keeps concurrent writers from interleaving.
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(file)
    {
        let _ = f.write_all(format!("{line}\n").as_bytes());
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
