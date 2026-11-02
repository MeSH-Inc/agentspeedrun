//! Regression tests for v0.2 acceptance items (docs/END_STATE.md, docs/V0.2.md), run
//! against the `speedrund` binary over loopback HTTP.
//!
//! Tests marked `#[ignore = "audit ..."]` fail until their item holds; run them with
//! `cargo test -p speedrund -- --ignored audit_`. The `guard_` tests pass today and keep a
//! fix from passing by removing the behaviour under test.

use std::fs::{self, File};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{Value, json};

/// How long the daemon may take to answer its first health check.
const STARTUP_LIMIT: Duration = Duration::from_secs(15);
/// How long a log line may take to reach stderr or a log file.
const LOG_LIMIT: Duration = Duration::from_secs(3);

/// A daemon with its own data directory on a free loopback port, killed on drop.
struct Daemon {
    child: Child,
    base: String,
    home: tempfile::TempDir,
    /// Holds the daemon's captured stdout and stderr, outside its data directory.
    output: tempfile::TempDir,
    token: String,
    display_token: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free loopback port")
}

/// Send one request; transport failures give status 0. Bodies that are not JSON give `Null`.
fn request(method: &str, url: &str, bearer: Option<&str>, body: Option<&Value>) -> (u16, Value) {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
    let mut req = agent.request(method, url);
    if let Some(token) = bearer {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let result = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(_)) => return (0, Value::Null),
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

impl Daemon {
    fn start() -> Self {
        for _ in 0..5 {
            if let Some(d) = Self::try_start() {
                return d;
            }
        }
        panic!("speedrund did not start on any of five free ports");
    }

    /// `None` when the daemon exits early (a port taken between probe and bind) or never answers.
    fn try_start() -> Option<Self> {
        let home = tempfile::tempdir().expect("temp data dir");
        let output = tempfile::tempdir().expect("temp output dir");
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_speedrund"))
            .env("SPEEDRUN_HOME", home.path())
            .env("SPEEDRUN_ADDR", format!("127.0.0.1:{port}"))
            .env_remove("SPEEDRUN_LOG")
            .env_remove("RUST_LOG")
            .env_remove("SPEEDRUN_DEVICE_KEY")
            .env_remove("SPEEDRUN_CORS_ORIGINS")
            .env_remove("SPEEDRUN_ALLOW_NON_LOOPBACK")
            .stdin(Stdio::null())
            .stdout(File::create(output.path().join("stdout")).expect("stdout file"))
            .stderr(File::create(output.path().join("stderr")).expect("stderr file"))
            .spawn()
            .expect("spawn speedrund");
        let mut daemon = Daemon {
            child,
            base: format!("http://127.0.0.1:{port}"),
            home,
            output,
            token: String::new(),
            display_token: String::new(),
        };
        let deadline = Instant::now() + STARTUP_LIMIT;
        while Instant::now() < deadline {
            if !matches!(daemon.child.try_wait(), Ok(None)) {
                return None;
            }
            if daemon.get("/v1/health", None).0 == 200 {
                let read = |name: &str| {
                    fs::read_to_string(daemon.home.path().join(name))
                        .expect("token written before the daemon listens")
                        .trim()
                        .to_string()
                };
                daemon.token = read("token");
                daemon.display_token = read("display-token");
                return Some(daemon);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    fn get(&self, path: &str, bearer: Option<&str>) -> (u16, Value) {
        request("GET", &format!("{}{path}", self.base), bearer, None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        request(
            "POST",
            &format!("{}{path}", self.base),
            Some(&self.token),
            Some(body),
        )
    }

    /// Record a prompt for `project`; returns the run it went to.
    fn prompt(&self, event_id: &str, project: &str) -> String {
        let (status, body) = self.post(
            "/v1/prompts",
            &json!({
                "event_id": event_id,
                "project_id": project,
                "project_root": null,
                "category_id": "any-percent",
                "session_id": "s1",
                "host": "test",
                "model": null,
                "content": "build it",
                "occurred_at": null,
            }),
        );
        assert_eq!(status, 200, "recording a prompt: {body}");
        body["run_id"].as_str().expect("run_id").to_string()
    }

    /// What the daemon wrote to stderr, plus every file in its data directory except the
    /// database: wherever a fix sends logs that the user can read later.
    fn diagnostics(&self) -> String {
        let mut out = fs::read_to_string(self.output.path().join("stderr")).unwrap_or_default();
        collect_text(self.home.path(), &mut out);
        out
    }

    fn stdout(&self) -> String {
        fs::read_to_string(self.output.path().join("stdout")).unwrap_or_default()
    }
}

fn collect_text(dir: &Path, out: &mut String) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_text(&path, out);
        } else if !entry.file_name().to_string_lossy().starts_with("speedrun.sqlite")
            && let Ok(bytes) = fs::read(&path)
        {
            out.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
}

fn wait_for(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn claim(milestone_id: &str, evidence: Value) -> Value {
    json!({
        "type": "milestone_reached",
        "milestone_id": milestone_id,
        "trust": "self_declared",
        "evidence": evidence,
    })
}

fn run_ids(snapshot: &Value) -> Vec<String> {
    snapshot["runs"]
        .as_array()
        .map(|runs| {
            runs.iter()
                .filter_map(|r| r["run_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// --- W1.b: one corrupt row ------------------------------------------------------------

/// Append a row to `run_id` whose body is not a readable event, the way a damaged or
/// newer-format database would hold one. The triggers forbid updates, not inserts.
fn insert_corrupt_row(home: &Path, run_id: &str, event_id: &str) {
    let conn = rusqlite::Connection::open(home.join("speedrun.sqlite")).expect("open the database");
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO events (event_id, run_id, seq, type, occurred_at, recorded_at, hash, body)
         VALUES (?1, ?2, (SELECT MAX(seq) + 1 FROM events WHERE run_id = ?2),
                 'milestone_reached', ?3, ?3, ?4, ?5)",
        rusqlite::params![
            event_id,
            run_id,
            now,
            "0".repeat(64),
            r#"{"event_id": "truncated mid-wr"#
        ],
    )
    .expect("insert the corrupt row");
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Bool(true) => false,
    }
}

/// The snapshot reports a problem somewhere: a non-empty value under a key that names an
/// error, or a mention of the unreadable row's event id.
fn reports_an_error(snapshot: &Value, event_id: &str) -> bool {
    fn error_key(v: &Value) -> bool {
        match v {
            Value::Object(o) => o
                .iter()
                .any(|(k, v)| (k.to_ascii_lowercase().contains("error") && !is_empty(v)) || error_key(v)),
            Value::Array(a) => a.iter().any(error_key),
            _ => false,
        }
    }
    error_key(snapshot) || snapshot.to_string().contains(event_id)
}

/// W1.b: one corrupt row yields a 200 from `/v1/live` with an error entry (V0.2 W1: live skips and reports rows it cannot parse).
#[test]
fn audit_w1_b_corrupt_row_gives_live_200_with_an_error_entry() {
    let d = Daemon::start();
    let healthy = d.prompt("healthy-p1", "healthy");
    let damaged = d.prompt("damaged-p1", "damaged");
    insert_corrupt_row(d.home.path(), &damaged, "corrupt-row-1");

    let (status, snapshot) = d.get("/v1/live", Some(&d.display_token));
    assert_eq!(
        status, 200,
        "one unreadable row must not fail the whole snapshot: {snapshot}"
    );
    assert!(
        run_ids(&snapshot).contains(&healthy),
        "the healthy project's run is still shown: {snapshot}"
    );
    assert!(
        reports_an_error(&snapshot, "corrupt-row-1"),
        "the unreadable row is reported, not silently dropped: {snapshot}"
    );
}

/// W1.b guard: with no corrupt rows, every run appears and no error entry is reported.
#[test]
fn guard_w1_b_healthy_log_lists_every_run_without_an_error_entry() {
    let d = Daemon::start();
    let a = d.prompt("a-p1", "a");
    let b = d.prompt("b-p1", "b");
    let (status, snapshot) = d.get("/v1/live", Some(&d.display_token));
    assert_eq!(status, 200, "{snapshot}");
    let ids = run_ids(&snapshot);
    assert!(ids.contains(&a) && ids.contains(&b), "{snapshot}");
    assert!(!reports_an_error(&snapshot, "no-such-event"), "{snapshot}");
}

// --- W2.c: backdated claims -------------------------------------------------------------

/// W2.c: a backdated claim gets 400 (V0.2 W2: occurred_at no earlier than the run's last event minus skew, unless a marked spool replay).
#[test]
fn audit_w2_c_backdated_claim_is_rejected() {
    let d = Daemon::start();
    let run = d.prompt("p1", "proj");
    let a_day_ago = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();

    let (by_run, body_run) = d.post(
        "/v1/events",
        &json!({
            "event_id": "backdated-1",
            "run_id": run,
            "occurred_at": a_day_ago,
            "payload": claim("scaffold-runs", json!([])),
        }),
    );
    let (by_project, body_project) = d.post(
        "/v1/projects/proj/events",
        &json!({
            "event_id": "backdated-2",
            "occurred_at": a_day_ago,
            "payload": claim("happy-path-demo", json!([])),
        }),
    );
    assert_eq!(
        (by_run, by_project),
        (400, 400),
        "claims dated a day before the run's last event: {body_run} / {body_project}"
    );
}

/// W2.c guard: a claim dated now, after the run's last event, is still accepted on both routes.
#[test]
fn guard_w2_c_current_claim_is_accepted() {
    let d = Daemon::start();
    let run = d.prompt("p1", "proj");

    let (by_run, body_run) = d.post(
        "/v1/events",
        &json!({
            "event_id": "current-1",
            "run_id": run,
            "occurred_at": Utc::now().to_rfc3339(),
            "payload": claim("scaffold-runs", json!([])),
        }),
    );
    let (by_project, body_project) = d.post(
        "/v1/projects/proj/events",
        &json!({
            "event_id": "current-2",
            "occurred_at": Utc::now().to_rfc3339(),
            "payload": claim("happy-path-demo", json!([])),
        }),
    );
    assert_eq!((by_run, by_project), (200, 200), "{body_run} / {body_project}");
    assert_eq!(body_run["inserted"], true, "{body_run}");
    assert_eq!(body_project["inserted"], true, "{body_project}");
}

// --- W4.c: the full token in a query string ----------------------------------------------

/// W4.c: `GET /v1/live?token=<full>` returns 401 (V0.2 W4: accept only the display token in `?token=`).
#[test]
fn audit_w4_c_full_token_in_query_string_is_rejected() {
    let d = Daemon::start();
    let (status, body) = d.get(&format!("/v1/live?token={}", d.token), None);
    assert_eq!(status, 401, "{body}");
}

/// W4.c guard: the display token in `?token=` still reads the live snapshot (browsers' EventSource).
#[test]
fn guard_w4_c_display_token_in_query_string_is_accepted() {
    let d = Daemon::start();
    let (status, body) = d.get(&format!("/v1/live?token={}", d.display_token), None);
    assert_eq!(status, 200, "{body}");
}

/// W4.c guard: the full token in the Authorization header still reads `/v1/live` (`speedrun watch`).
#[test]
fn guard_w4_c_full_token_in_header_reads_live() {
    let d = Daemon::start();
    let (status, body) = d.get("/v1/live", Some(&d.token));
    assert_eq!(status, 200, "{body}");
}

// --- M3: daemon logs --------------------------------------------------------------------

/// M3: daemon logs reach stderr or a capped file (V0.2 M3: stdout is discarded by autostart; log data_dir at startup).
#[test]
fn audit_m3_daemon_logs_reach_stderr_or_a_file() {
    let d = Daemon::start();
    let data_dir = d.home.path().to_string_lossy().into_owned();
    let startup_logged = wait_for(LOG_LIMIT, || d.diagnostics().contains(&data_dir));

    // A failed verification is one of the lines A5-2 found nowhere: a commit with no project root.
    let before = d.diagnostics().len();
    let run = d.prompt("p1", "proj");
    let (status, body) = d.post(
        "/v1/events",
        &json!({
            "event_id": "unverifiable",
            "run_id": run,
            "occurred_at": null,
            "payload": claim("happy-path-demo", json!([{"kind": "git_commit", "sha": "a".repeat(40)}])),
        }),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["verification"]["verified"], false, "{body}");
    let runtime_logged = wait_for(LOG_LIMIT, || d.diagnostics().len() > before);

    assert!(
        startup_logged && runtime_logged,
        "startup line naming the data dir on stderr or in a file: {startup_logged}; \
         failed verification logged there: {runtime_logged}\n--- stderr and data-dir files:\n{}\n--- stdout:\n{}",
        d.diagnostics(),
        d.stdout()
    );
}

// --- W1.a, A2-5: the daemon verifies stored bodies, so a newer build's fields verify ----

/// sha256 over the canonical JSON of `body` without `hash` and `sig`.
fn stored_hash(body: &Value) -> String {
    use sha2::Digest;
    let mut v = body.clone();
    let map = v.as_object_mut().expect("an event body is an object");
    map.remove("hash");
    map.remove("sig");
    hex::encode(sha2::Sha256::digest(
        speedrun_core::hash::canonical_json(&v).as_bytes(),
    ))
}

/// Append to `run_id`, behind the daemon's back, the event a newer build would write: a
/// payload with an optional field this build does not know, hashed over the stored body and
/// signed with this data directory's device key.
fn insert_event_from_a_newer_build(home: &Path, run_id: &str) {
    let conn = rusqlite::Connection::open(home.join("speedrun.sqlite")).expect("open the database");
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    let (seq, last_body): (i64, String) = conn
        .query_row(
            "SELECT seq, body FROM events WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the run has events");
    let last: Value = serde_json::from_str(&last_body).unwrap();
    let now = Utc::now().to_rfc3339();
    let mut body = last.clone();
    body["event_id"] = json!("from-a-newer-build");
    body["seq"] = json!(seq + 1);
    body["occurred_at"] = json!(now);
    body["recorded_at"] = json!(now);
    body["payload"] = json!({
        "type": "agent_turn_ended",
        "session_id": "s1",
        "future_field": {"added_in": "a later schema"},
    });
    body["prev_hash"] = last["hash"].clone();
    body["hash"] = json!(stored_hash(&body));
    let key = speedrun_core::sign::parse_seed(
        &fs::read_to_string(home.join("device.key")).expect("the device key"),
    )
    .expect("a device key seed");
    body["sig"] = json!(speedrun_core::sign::sign_hash(
        &key,
        body["hash"].as_str().unwrap()
    ));
    conn.execute(
        "INSERT INTO events (event_id, run_id, seq, type, occurred_at, recorded_at, hash, body)
         VALUES (?1, ?2, ?3, 'agent_turn_ended', ?4, ?4, ?5, ?6)",
        rusqlite::params![
            "from-a-newer-build",
            run_id,
            seq + 1,
            now,
            body["hash"].as_str().unwrap(),
            body.to_string()
        ],
    )
    .expect("insert the event");
}

#[test]
fn audit_w1_a_event_with_an_unknown_optional_field_verifies() {
    let d = Daemon::start();
    let run = d.prompt("p1", "proj");
    insert_event_from_a_newer_build(d.home.path(), &run);
    let (status, report) = d.get(&format!("/v1/runs/{run}/verify"), Some(&d.token));
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["ok"], true, "{report}");
}

#[test]
fn guard_w1_a_edited_event_still_fails_verification() {
    let d = Daemon::start();
    let run = d.prompt("p1", "proj");
    insert_event_from_a_newer_build(d.home.path(), &run);
    // A second row that claims the first one's hash for different contents.
    let conn = rusqlite::Connection::open(d.home.path().join("speedrun.sqlite")).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    let (seq, body): (i64, String) = conn
        .query_row(
            "SELECT seq, body FROM events WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
            [&run],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let mut forged: Value = serde_json::from_str(&body).unwrap();
    forged["event_id"] = json!("forged");
    forged["seq"] = json!(seq + 1);
    forged["prev_hash"] = forged["hash"].clone();
    forged["payload"]["future_field"] = json!("edited without rehashing");
    conn.execute(
        "INSERT INTO events (event_id, run_id, seq, type, occurred_at, recorded_at, hash, body)
         VALUES ('forged', ?1, ?2, 'agent_turn_ended', ?3, ?3, ?4, ?5)",
        rusqlite::params![
            run,
            seq + 1,
            Utc::now().to_rfc3339(),
            forged["hash"].as_str().unwrap(),
            forged.to_string()
        ],
    )
    .unwrap();
    let (_, report) = d.get(&format!("/v1/runs/{run}/verify"), Some(&d.token));
    assert_eq!(report["ok"], false, "{report}");
}

// --- W1.c, A5-4: a binary never runs against a database written by a newer one ----------

#[test]
fn audit_w1_c_database_with_a_newer_schema_stops_the_daemon_with_a_clear_line() {
    let home = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    {
        let conn = rusqlite::Connection::open(home.path().join("speedrun.sqlite")).unwrap();
        conn.execute_batch("CREATE TABLE future (x); PRAGMA user_version = 99;")
            .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_speedrund"))
        .env("SPEEDRUN_HOME", home.path())
        .env("SPEEDRUN_ADDR", format!("127.0.0.1:{}", free_port()))
        .env_remove("SPEEDRUN_LOG")
        .env_remove("RUST_LOG")
        .env_remove("SPEEDRUN_DEVICE_KEY")
        .stdin(Stdio::null())
        .stdout(File::create(output.path().join("stdout")).unwrap())
        .stderr(File::create(output.path().join("stderr")).unwrap())
        .spawn()
        .unwrap();
    let mut status = None;
    wait_for(STARTUP_LIMIT, || {
        status = child.try_wait().ok().flatten();
        status.is_some()
    });
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let stderr = fs::read_to_string(output.path().join("stderr")).unwrap_or_default();
    assert!(
        status.is_some_and(|s| !s.success()),
        "the daemon must exit non-zero, not serve a newer database (exit: {status:?})\n{stderr}"
    );
    assert!(
        stderr.contains("99") && stderr.to_lowercase().contains("newer"),
        "the log line must say the database is newer and name its version:\n{stderr}"
    );
    let version: i64 = rusqlite::Connection::open(home.path().join("speedrun.sqlite"))
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 99, "the newer database must be left as it was");
}

// --- W2.d, A2-6: a marked spool replay is exempt from the last-event bound ---------------

/// Header a client sets when it delivers an event from its spool.
const REPLAY_HEADER: &str = "X-Speedrun-Replay";

fn post_replay(d: &Daemon, path: &str, body: &Value) -> (u16, Value) {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
    let result = agent
        .post(&format!("{}{path}", d.base))
        .set("Authorization", &format!("Bearer {}", d.token))
        .set(REPLAY_HEADER, "1")
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(_)) => return (0, Value::Null),
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn turn_end(event_id: &str, at: chrono::DateTime<Utc>) -> Value {
    json!({
        "event_id": event_id,
        "occurred_at": at.to_rfc3339(),
        "payload": {"type": "agent_turn_ended", "session_id": "s1"},
    })
}

/// A run whose newest event is a claim made now, after a prompt made 40 minutes ago.
fn run_with_a_recent_event(d: &Daemon) -> String {
    let (status, body) = d.post(
        "/v1/prompts",
        &json!({
            "event_id": "p1", "project_id": "proj", "project_root": null,
            "category_id": "any-percent", "session_id": "s1", "host": "test", "model": null,
            "content": "build it",
            "occurred_at": (Utc::now() - chrono::Duration::minutes(40)).to_rfc3339(),
        }),
    );
    assert_eq!(status, 200, "{body}");
    let run = body["run_id"].as_str().unwrap().to_string();
    let (status, body) = d.post(
        "/v1/events",
        &json!({
            "event_id": "recent", "run_id": run, "occurred_at": Utc::now().to_rfc3339(),
            "payload": claim("scaffold-runs", json!([])),
        }),
    );
    assert_eq!(status, 200, "{body}");
    run
}

#[test]
fn audit_w2_d_marked_replay_older_than_the_last_event_is_accepted() {
    let d = Daemon::start();
    let _run = run_with_a_recent_event(&d);
    let spooled_at = Utc::now() - chrono::Duration::minutes(30);
    let (status, body) = post_replay(&d, "/v1/projects/proj/events", &turn_end("spooled", spooled_at));
    assert_eq!(
        status, 200,
        "a spooled event delivered late must be recorded: {body}"
    );
    let recorded = body["event"]["occurred_at"].as_str().unwrap_or_default();
    let recorded = chrono::DateTime::parse_from_rfc3339(recorded).map(|t| t.with_timezone(&Utc));
    assert_eq!(
        recorded.ok().map(|t| t.timestamp()),
        Some(spooled_at.timestamp()),
        "at the time it happened: {body}"
    );
}

#[test]
fn guard_w2_d_unmarked_event_older_than_the_last_event_is_refused() {
    let d = Daemon::start();
    let _run = run_with_a_recent_event(&d);
    let (status, body) = d.post(
        "/v1/projects/proj/events",
        &turn_end("unmarked", Utc::now() - chrono::Duration::minutes(30)),
    );
    assert_eq!(status, 400, "{body}");
}

#[test]
fn guard_w2_d_replay_from_before_the_run_started_is_refused() {
    let d = Daemon::start();
    let _run = run_with_a_recent_event(&d);
    let (status, body) = post_replay(
        &d,
        "/v1/projects/proj/events",
        &turn_end("before-start", Utc::now() - chrono::Duration::hours(2)),
    );
    assert_eq!(status, 400, "a replay may not predate its run: {body}");
}

#[test]
fn audit_w2_d_prompt_older_than_the_last_event_is_refused() {
    let d = Daemon::start();
    let _run = run_with_a_recent_event(&d);
    let (status, body) = d.post(
        "/v1/prompts",
        &json!({
            "event_id": "late-prompt", "project_id": "proj", "project_root": null,
            "category_id": "any-percent", "session_id": "s1", "host": "test", "model": null,
            "content": "and then",
            "occurred_at": (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339(),
        }),
    );
    assert_eq!(
        status, 400,
        "an unmarked prompt dated before the run's last event: {body}"
    );
}

#[test]
fn guard_w2_d_marked_prompt_replay_is_accepted() {
    let d = Daemon::start();
    let _run = run_with_a_recent_event(&d);
    let (status, body) = post_replay(
        &d,
        "/v1/prompts",
        &json!({
            "event_id": "spooled-prompt", "project_id": "proj", "project_root": null,
            "category_id": "any-percent", "session_id": "s1", "host": "test", "model": null,
            "content": "and then",
            "occurred_at": (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339(),
        }),
    );
    assert_eq!(status, 200, "{body}");
}

/// W2.d guard: a retry of an event already recorded gets its original answer, even once the
/// run has moved more than the skew allowance past it. Idempotency comes before the bound.
#[test]
fn guard_w2_d_retry_of_a_recorded_event_is_answered_not_refused() {
    let d = Daemon::start();
    let forty_ago = (Utc::now() - chrono::Duration::minutes(40)).to_rfc3339();
    let thirty_ago = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
    let prompt = json!({
        "event_id": "p1", "project_id": "proj", "project_root": null,
        "category_id": "any-percent", "session_id": "s1", "host": "test", "model": null,
        "content": "build it", "occurred_at": forty_ago,
    });
    let (status, body) = d.post("/v1/prompts", &prompt);
    assert_eq!(status, 200, "{body}");
    let run = body["run_id"].as_str().unwrap().to_string();
    let by_run = json!({
        "event_id": "early-by-run", "run_id": run, "occurred_at": thirty_ago,
        "payload": claim("scaffold-runs", json!([])),
    });
    let by_project = json!({
        "event_id": "early-by-project", "occurred_at": thirty_ago,
        "payload": {"type": "agent_turn_ended", "session_id": "s1"},
    });
    assert_eq!(d.post("/v1/events", &by_run).0, 200);
    assert_eq!(d.post("/v1/projects/proj/events", &by_project).0, 200);
    let (status, body) = d.post(
        "/v1/events",
        &json!({
            "event_id": "now", "run_id": run, "occurred_at": Utc::now().to_rfc3339(),
            "payload": claim("happy-path-demo", json!([])),
        }),
    );
    assert_eq!(status, 200, "{body}");

    let retries = [
        ("/v1/prompts", &prompt),
        ("/v1/events", &by_run),
        ("/v1/projects/proj/events", &by_project),
    ];
    for (path, request) in retries {
        let (status, body) = d.post(path, request);
        assert_eq!(status, 200, "retry on {path}: {body}");
    }
}
