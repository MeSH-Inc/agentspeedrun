//! Regression tests for v0.1.0 audit findings (docs/audit/v0.1.0.json, docs/V0.2.md).
//!
//! Each `audit_` test was ignored until its finding was fixed, then made an ordinary test.
//! The `guard_` tests keep a fix from passing by removing the behaviour under test.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Run the `speedrun` binary in an empty data directory, home and working directory.
fn speedrun(args: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_speedrun"))
        .args(args)
        .current_dir(dir.path())
        .env("SPEEDRUN_HOME", dir.path())
        .env("HOME", dir.path())
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn exit_code(args: &[&str]) -> Option<i32> {
    speedrun(args).status.code()
}

// --- A4-1, V0.2 M1: a hook that cannot parse its arguments blocks Claude Code ------------

#[test]
fn audit_a4_1_hook_with_unknown_arguments_exits_zero() {
    assert_eq!(exit_code(&["hook", "x", "y", "--bogus"]), Some(0));
}

#[test]
fn audit_a4_1_hook_with_a_flag_from_a_newer_plugin_exits_zero() {
    assert_eq!(
        exit_code(&["hook", "claude-code", "user-prompt-submit", "--added-later"]),
        Some(0)
    );
}

#[test]
fn audit_a4_1_hook_with_missing_arguments_exits_zero() {
    assert_eq!(exit_code(&["hook", "claude-code"]), Some(0));
}

#[test]
fn audit_a4_1_ingest_with_bad_arguments_exits_zero() {
    assert_eq!(exit_code(&["ingest", "prompt", "--bogus"]), Some(0));
}

#[test]
fn audit_a4_1_ingest_with_bad_arguments_still_prints_its_json_object() {
    // Adapters parse ingest's stdout: it is one JSON object even when nothing was recorded.
    let out = speedrun(&["ingest", "prompt", "--bogus"]);
    let reply: serde_json::Value = serde_json::from_slice(&out.stdout).expect("one JSON object");
    assert_eq!(reply["recorded"], false);
    assert!(reply["error"].is_string());
}

#[test]
fn guard_hook_for_an_unknown_host_exits_zero() {
    assert_eq!(exit_code(&["hook", "x", "y"]), Some(0));
}

#[test]
fn guard_other_commands_still_reject_bad_arguments() {
    assert_ne!(exit_code(&["status", "--bogus"]), Some(0));
    assert_ne!(exit_code(&["no-such-command"]), Some(0));
}

// --- Machinery for tests that spool, flush and replay -----------------------------------

/// One machine: its own data directory, home and tracked project. Its daemon address is a
/// free loopback port where nothing answers until a test starts a daemon there, and the
/// client never starts one itself.
struct Machine {
    root: tempfile::TempDir,
    addr: String,
}

/// The project every machine tracks, as its committed `.speedrun/project.json` names it.
const PROJECT: &str = "01KAUDITPROJECT0000000000";

impl Machine {
    /// A machine whose user has trusted its project (V0.2 W6): the allowlist in the data
    /// directory names the project id and its root.
    fn new() -> Self {
        let machine = Self::untrusted();
        let root = machine.project_root().canonicalize().unwrap();
        let entry = serde_json::json!([{ "project_id": PROJECT, "root": root }]);
        std::fs::write(
            machine.data_dir().join("trusted-projects.json"),
            entry.to_string(),
        )
        .unwrap();
        machine
    }

    /// A machine with a cloned project the user has not trusted yet.
    fn untrusted() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("project/.speedrun");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("project.json"),
            r#"{"project_id":"01KAUDITPROJECT0000000000","category_id":"any-percent","created_at":"2026-11-01T00:00:00Z"}"#,
        )
        .unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("token"), "audit-test-token").unwrap();
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string();
        Self { root, addr }
    }

    fn data_dir(&self) -> PathBuf {
        self.root.path().join("data")
    }

    fn project_root(&self) -> PathBuf {
        self.root.path().join("project")
    }

    fn speedrun(&self, args: &[&str], stdin: &[u8]) -> Output {
        self.run_in(&self.project_root(), args, stdin, true)
    }

    /// Run `speedrun` in `dir`. Without `addr`, the client is not told the daemon's
    /// address and must find it itself.
    fn run_in(&self, dir: &Path, args: &[&str], stdin: &[u8], addr: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_speedrun"));
        command
            .args(args)
            .current_dir(dir)
            .env("SPEEDRUN_HOME", self.data_dir())
            .env("SPEEDRUN_NO_AUTOSTART", "1")
            .env("HOME", self.root.path())
            .env_remove("XDG_DATA_HOME");
        if addr {
            command.env("SPEEDRUN_ADDR", &self.addr);
        } else {
            command.env_remove("SPEEDRUN_ADDR");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // A command that does not read stdin closes the pipe early; that is not an error here.
        let _ = child.stdin.take().unwrap().write_all(stdin);
        child.wait_with_output().unwrap()
    }

    fn prompt(&self, text: &str) -> Output {
        self.speedrun(
            &["ingest", "prompt", "--host", "test", "--session", "s"],
            text.as_bytes(),
        )
    }

    /// Every file the client keeps in its data directory, with its contents.
    fn stored_files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.push((path, bytes));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.data_dir(), &mut out);
        out
    }

    /// The stored files whose contents include `text`.
    fn files_containing(&self, text: &str) -> Vec<PathBuf> {
        self.stored_files()
            .into_iter()
            .filter(|(_, bytes)| contains(bytes, text))
            .map(|(path, _)| path)
            .collect()
    }
}

fn contains(bytes: &[u8], text: &str) -> bool {
    bytes.windows(text.len()).any(|w| w == text.as_bytes())
}

/// The token every machine's data directory holds.
const TOKEN: &str = "audit-test-token";

/// How a stand-in daemon answers.
#[derive(Clone, Copy)]
struct Behaviour {
    /// Health proves the machine's token (`speedrun_core::digest::token_proof`); when
    /// false, it proves a different token, as a daemon serving another data dir would.
    proves_token: bool,
    /// Status line for every request but health.
    status: &'static str,
    /// Time taken to answer a request to the project events route, where the spool's
    /// backlog goes.
    backlog_delay: Duration,
}

/// One request a stand-in daemon received: its request line and body.
#[derive(Clone)]
struct Received {
    line: String,
    body: Vec<u8>,
}

/// A stand-in daemon on a machine's address: healthy, but it refuses every other request
/// with 400, as speedrund refuses an event its run no longer accepts. It keeps the body
/// of every request it refused.
struct RefusingDaemon {
    received: Arc<Mutex<Vec<Received>>>,
    /// The request lines, health included, of every request that carried the token.
    shown_token: Arc<Mutex<Vec<String>>>,
}

impl RefusingDaemon {
    fn start(machine: &Machine) -> Self {
        Self::behaving(
            machine,
            Behaviour {
                proves_token: true,
                status: "400 Bad Request",
                backlog_delay: Duration::ZERO,
            },
        )
    }

    fn behaving(machine: &Machine, behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind(&machine.addr).unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let shown_token = Arc::new(Mutex::new(Vec::new()));
        let (record, shown) = (Arc::clone(&received), Arc::clone(&shown_token));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (record, shown) = (Arc::clone(&record), Arc::clone(&shown));
                std::thread::spawn(move || answer(stream, &record, &shown, behaviour));
            }
        });
        Self {
            received,
            shown_token,
        }
    }

    /// The request lines of every request, health included, that carried the token.
    fn shown_token(&self) -> Vec<String> {
        self.shown_token.lock().unwrap().clone()
    }

    /// Every request but health, in the order they arrived.
    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn refused(&self) -> Vec<Vec<u8>> {
        self.received().into_iter().map(|r| r.body).collect()
    }
}

fn answer(
    stream: TcpStream,
    received: &Mutex<Vec<Received>>,
    shown_token: &Mutex<Vec<String>>,
    behaviour: Behaviour,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let (mut length, mut chunked) = (0, false);
    let mut carries_token = request_line.contains(TOKEN);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        carries_token |= line.contains(TOKEN);
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "transfer-encoding" => chunked = value.trim().eq_ignore_ascii_case("chunked"),
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size)?;
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk)?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else {
        body.resize(length, 0);
        reader.read_exact(&mut body)?;
    }
    if carries_token || contains(&body, TOKEN) {
        shown_token
            .lock()
            .unwrap()
            .push(request_line.trim_end().to_string());
    }
    let target = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let (status, reply) = if target == "/v1/health" || target.starts_with("/v1/health?") {
        let nonce = target
            .split_once("nonce=")
            .map(|(_, n)| n.split('&').next().unwrap_or_default());
        let token = if behaviour.proves_token {
            TOKEN
        } else {
            "another-data-dirs-token"
        };
        let mut health = serde_json::json!({
            "ok": true,
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": speedrun_core::api::PROTOCOL_VERSION,
            "data_dir_id": "0".repeat(64),
        });
        if let Some(nonce) = nonce {
            health["proof"] = speedrun_core::digest::token_proof(token, nonce).into();
        }
        ("200 OK", health.to_string())
    } else {
        received.lock().unwrap().push(Received {
            line: request_line.trim_end().to_string(),
            body,
        });
        if target.starts_with("/v1/projects/") {
            std::thread::sleep(behaviour.backlog_delay);
        }
        let reply = if behaviour.status.starts_with('2') {
            "{}".to_string()
        } else {
            r#"{"error":"refused by the stand-in daemon"}"#.to_string()
        };
        (behaviour.status, reply)
    };
    // The response in one write, sent without Nagle's delay.
    let mut stream = stream;
    stream.set_nodelay(true)?;
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// The real speedrund, built beside the `speedrun` binary, serving a machine's data
/// directory. Killed when dropped, which is how these tests make an outage.
struct Daemon(std::process::Child);

impl Daemon {
    fn start(machine: &Machine) -> Self {
        let exe = Path::new(env!("CARGO_BIN_EXE_speedrun")).with_file_name("speedrund");
        assert!(
            exe.exists(),
            "{} is missing; run `cargo build -p speedrund`",
            exe.display()
        );
        let child = Command::new(exe)
            .env("SPEEDRUN_HOME", machine.data_dir())
            .env("SPEEDRUN_ADDR", &machine.addr)
            .env("HOME", machine.root.path())
            .env_remove("XDG_DATA_HOME")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(&machine.addr).is_err() {
            assert!(Instant::now() < deadline, "speedrund did not start listening");
            std::thread::sleep(Duration::from_millis(20));
        }
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// --- A1-3, V0.2 M7: the spool and rejected.jsonl hold plaintext secrets forever ----------

/// A secret shape the redactor already handles (`speedrun_core::redact`).
const SECRET: &str = "sk_live_FAKEFAKEFAKEFAKE0123456789";

#[test]
fn audit_m7_b_spooled_prompt_is_redacted() {
    // END_STATE M7.b; V0.2 M7: the client calls redact() before spooling.
    let machine = Machine::new();
    machine.prompt(&format!("charge the card with STRIPE key {SECRET} and ship it"));
    let leaks = machine.files_containing(SECRET);
    assert!(leaks.is_empty(), "the secret is stored in {leaks:?}");
}

#[test]
fn guard_spooled_prompt_without_secrets_is_kept_intact() {
    let machine = Machine::new();
    let text = "add a todo list with drag and drop ordering";
    machine.prompt(text);
    assert!(
        !machine.files_containing(text).is_empty(),
        "the prompt was not spooled"
    );
}

#[test]
fn audit_m7_b_sent_prompt_is_redacted() {
    // END_STATE M7.b; V0.2 M7: the client calls redact() before sending.
    let machine = Machine::new();
    let daemon = RefusingDaemon::start(&machine);
    machine.prompt(&format!("charge the card with STRIPE key {SECRET} and ship it"));
    let sent = daemon.refused();
    assert!(!sent.is_empty(), "the prompt never reached the daemon");
    assert!(
        !sent.iter().any(|body| contains(body, SECRET)),
        "the secret was sent to the daemon"
    );
}

#[test]
fn guard_sent_prompt_without_secrets_is_sent_intact() {
    let machine = Machine::new();
    let daemon = RefusingDaemon::start(&machine);
    let text = "add a todo list with drag and drop ordering";
    machine.prompt(text);
    assert!(daemon.refused().iter().any(|body| contains(body, text)));
}

#[test]
fn audit_m7_b_refused_spool_entries_do_not_accumulate_without_bound() {
    // END_STATE M7.b; V0.2 M7: rejected.jsonl is capped. V0.2 states no size, so the bound
    // is generous: after 256 refusals of 64 KiB prompts (16 MiB), the data directory keeps
    // at most half of what was refused, which a cap of up to 128 entries or 8 MiB meets.
    // Few large prompts rather than many small ones: each refused request costs the
    // client tens of milliseconds, and this test runs in every ratchet measurement.
    const PROMPTS: usize = 256;
    let machine = Machine::new();
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let machine = &machine;
            scope.spawn(move || {
                for i in (worker..PROMPTS).step_by(8) {
                    let line = format!("prompt {i}: add pagination to the list view. ");
                    machine.prompt(&line.repeat(64 * 1024 / line.len()));
                }
            });
        }
    });
    let daemon = RefusingDaemon::start(&machine);
    for _ in 0..20 {
        if daemon.refused().len() >= PROMPTS {
            break;
        }
        machine.speedrun(&["flush"], b"");
    }
    let refused = daemon.refused();
    assert_eq!(refused.len(), PROMPTS, "every spooled prompt was refused once");
    let refused_bytes: usize = refused.iter().map(Vec::len).sum();
    let kept_bytes: usize = machine.stored_files().iter().map(|(_, b)| b.len()).sum();
    assert!(
        kept_bytes <= refused_bytes / 2,
        "{kept_bytes} bytes kept after {refused_bytes} bytes were refused"
    );
}

#[test]
fn guard_refused_spool_entry_is_kept_for_inspection() {
    let machine = Machine::new();
    let text = "add a todo list with drag and drop ordering";
    machine.prompt(text);
    let daemon = RefusingDaemon::start(&machine);
    machine.speedrun(&["flush"], b"");
    assert_eq!(daemon.refused().len(), 1);
    assert!(
        !machine.files_containing(text).is_empty(),
        "the refused prompt was not kept"
    );
}

// --- A4-5, V0.2 W5: propose_milestones duplicates milestones when retried ---------------

/// Start a run, then propose each batch of titles with the daemon down, then bring it
/// back, deliver the spool and return the titles of the run's milestones.
fn titles_after_proposing_during_an_outage(batches: &[&[&str]]) -> Vec<String> {
    let machine = Machine::new();
    let daemon = Daemon::start(&machine);
    let started: serde_json::Value =
        serde_json::from_slice(&machine.prompt("build a todo app with accounts").stdout).unwrap();
    assert_eq!(started["run_started"], true, "{started}");
    drop(daemon);
    for titles in batches {
        let milestones: Vec<_> = titles
            .iter()
            .map(|title| serde_json::json!({ "title": title }))
            .collect();
        let args = serde_json::json!({ "milestones": milestones }).to_string();
        machine.speedrun(&["tool", "speedrun_propose_milestones", "--args", &args], b"");
    }
    let _daemon = Daemon::start(&machine);
    machine.speedrun(&["flush"], b"");
    let status = machine.speedrun(&["status", "--json"], b"");
    let run: serde_json::Value = serde_json::from_slice(&status.stdout)
        .unwrap_or_else(|e| panic!("status --json: {e}: {}", String::from_utf8_lossy(&status.stderr)));
    run["milestones"]
        .as_array()
        .expect("milestones")
        .iter()
        .filter_map(|m| m["title"].as_str().map(str::to_string))
        .collect()
}

fn count(titles: &[String], title: &str) -> usize {
    titles.iter().filter(|t| *t == title).count()
}

#[test]
fn audit_w5_b_repeated_propose_during_an_outage_gives_no_duplicate_milestones() {
    // END_STATE W5.b; V0.2 W5: "A repeated propose during an outage gives no duplicate milestones."
    let batch: &[&str] = &["login works", "todos persist across restarts"];
    let titles = titles_after_proposing_during_an_outage(&[batch, batch]);
    for title in batch {
        assert_eq!(count(&titles, title), 1, "{title:?} in {titles:?}");
    }
}

#[test]
fn guard_proposals_with_different_titles_during_an_outage_all_arrive() {
    let titles = titles_after_proposing_during_an_outage(&[&["login works"], &["todos persist"]]);
    assert_eq!(count(&titles, "login works"), 1, "{titles:?}");
    assert_eq!(count(&titles, "todos persist"), 1, "{titles:?}");
}

// --- W2.d, A2-6: flush marks its deliveries as replays ------------------------------------

/// POST to the machine's daemon with its token; the status and JSON body.
fn post(machine: &Machine, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    let result = ureq::post(&format!("http://{}{path}", machine.addr))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .send_json(body.clone());
    match result {
        Ok(r) => (r.status(), r.into_json().unwrap_or_default()),
        Err(ureq::Error::Status(s, r)) => (s, r.into_json().unwrap_or_default()),
        Err(e) => panic!("daemon unreachable: {e}"),
    }
}

#[test]
fn audit_w2_d_spooled_event_older_than_the_runs_last_event_is_delivered_by_flush() {
    let machine = Machine::new();
    let _daemon = Daemon::start(&machine);
    let now = chrono::Utc::now();
    let (status, body) = post(
        &machine,
        "/v1/prompts",
        &serde_json::json!({
            "event_id": "p1", "project_id": PROJECT, "project_root": null,
            "category_id": "any-percent", "session_id": "s", "host": "test", "model": null,
            "content": "build it",
            "occurred_at": (now - chrono::Duration::minutes(40)).to_rfc3339(),
        }),
    );
    assert_eq!(status, 200, "{body}");
    let run = body["run_id"].as_str().unwrap().to_string();
    let (status, body) = post(
        &machine,
        &format!("/v1/projects/{PROJECT}/events"),
        &serde_json::json!({
            "event_id": "recent", "occurred_at": now.to_rfc3339(),
            "payload": {"type": "milestone_reached", "milestone_id": "scaffold-runs",
                        "trust": "self_declared", "evidence": []},
        }),
    );
    assert_eq!(status, 200, "{body}");

    // A turn end the client spooled 30 minutes ago, while the daemon was unreachable.
    let spool = machine.data_dir().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let entry = serde_json::json!({
        "path": format!("/v1/projects/{PROJECT}/events"),
        "body": {
            "event_id": "spooled-turn-end",
            "occurred_at": (now - chrono::Duration::minutes(30)).to_rfc3339(),
            "payload": {"type": "agent_turn_ended", "session_id": "s"},
        },
    });
    std::fs::write(spool.join("pending.jsonl"), format!("{entry}\n")).unwrap();

    let out = machine.speedrun(&["flush"], b"");
    let events = ureq::get(&format!("http://{}/v1/runs/{run}/events", machine.addr))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(
        events.contains("spooled-turn-end"),
        "flush must deliver it as a replay; flush said: {}\nrejected: {:?}",
        String::from_utf8_lossy(&out.stdout),
        machine.files_containing("spooled-turn-end")
    );
}

#[test]
fn guard_an_unreadable_spool_line_loses_none_of_the_entries_after_it() {
    // A line that is not UTF-8 used to end the read, and the spool file was then deleted
    // with every later entry still in it.
    let machine = Machine::new();
    let _daemon = Daemon::start(&machine);
    let now = chrono::Utc::now();
    let (status, body) = post(
        &machine,
        "/v1/prompts",
        &serde_json::json!({
            "event_id": "p1", "project_id": PROJECT, "project_root": null,
            "category_id": "any-percent", "session_id": "s", "host": "test", "model": null,
            "content": "build it", "occurred_at": (now - chrono::Duration::minutes(10)).to_rfc3339(),
        }),
    );
    assert_eq!(status, 200, "{body}");
    let run = body["run_id"].as_str().unwrap().to_string();

    let spool = machine.data_dir().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let entry = serde_json::json!({
        "path": format!("/v1/projects/{PROJECT}/events"),
        "body": {
            "event_id": "after-the-bad-line",
            "occurred_at": now.to_rfc3339(),
            "payload": {"type": "agent_turn_ended", "session_id": "s"},
        },
    });
    let mut bytes = b"{\"path\": \"\xff\xfe\"}\n".to_vec();
    bytes.extend_from_slice(format!("{entry}\n").as_bytes());
    std::fs::write(spool.join("pending.jsonl"), bytes).unwrap();

    machine.speedrun(&["flush"], b"");
    let events = ureq::get(&format!("http://{}/v1/runs/{run}/events", machine.addr))
        .set("Authorization", &format!("Bearer {TOKEN}"))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(events.contains("after-the-bad-line"), "{events}");
    let kept = std::fs::read(spool.join("rejected.jsonl")).unwrap_or_default();
    assert!(
        kept.windows(2).any(|w| w == b"\xff\xfe"),
        "the unreadable line must be kept for inspection"
    );
}

// --- Wave 2b: export and key rotation from the command line ------------------------------

#[test]
fn audit_w9_d_speedrun_export_prints_commitments_and_no_prompt_text() {
    let machine = Machine::new();
    let _daemon = Daemon::start(&machine);
    let recorded = machine.prompt("add a login page with passkeys");
    assert!(recorded.status.success());
    let out = machine.speedrun(&["export", "--tier", "2"], b"");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!stdout.contains("login page"), "{stdout}");
    let export: serde_json::Value = serde_json::from_str(&stdout).expect("the export is JSON");
    let commitment = export["prompts"][0]["commitment"].as_str().unwrap_or_default();
    assert!(
        commitment.len() == 64 && commitment.bytes().all(|b| b.is_ascii_hexdigit()),
        "{stdout}"
    );
}

#[test]
fn audit_w9_c_speedrun_key_rotate_replaces_the_signing_key() {
    let machine = Machine::new();
    let _daemon = Daemon::start(&machine);
    assert!(machine.prompt("start").status.success());
    let before = std::fs::read_to_string(machine.data_dir().join("device.key")).unwrap();
    let out = machine.speedrun(&["key", "rotate"], b"");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(machine.data_dir().join("device.key")).unwrap();
    assert_ne!(before.trim(), after.trim());
    let verify = machine.speedrun(&["verify"], b"");
    assert!(
        machine.prompt("continue").status.success() && verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
}

// --- Wave 3a: transport, spooling, doctor and project trust (V0.2 M2, M7, M8, W4, W6) ---

fn stand_in(proves_token: bool, status: &'static str, backlog_delay: Duration) -> Behaviour {
    Behaviour {
        proves_token,
        status,
        backlog_delay,
    }
}

impl Machine {
    /// The spool files (pending or being flushed) whose contents include `text`. Not
    /// `rejected.jsonl`, which holds what the daemon refused for good.
    fn spooled_containing(&self, text: &str) -> Vec<PathBuf> {
        self.files_containing(text)
            .into_iter()
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                p.parent().is_some_and(|d| d.ends_with("spool"))
                    && (name.starts_with("pending") || name.starts_with("flushing"))
            })
            .collect()
    }

    /// The run the machine's real daemon holds for the project, or `Null`.
    fn active_run(&self) -> serde_json::Value {
        let body: serde_json::Value =
            ureq::get(&format!("http://{}/v1/projects/{PROJECT}/active-run", self.addr))
                .set("Authorization", &format!("Bearer {TOKEN}"))
                .call()
                .unwrap()
                .into_json()
                .unwrap();
        body["run"].clone()
    }

    fn doctor(&self) -> (Output, String) {
        let out = self.speedrun(&["doctor"], b"");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        (out, stdout)
    }
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l.trim() == line)
}

#[test]
#[ignore = "audit D3"]
fn audit_w4_a_client_finds_the_daemon_through_the_addr_file() {
    // V0.2 W4 / D3: clients read `$DATA_DIR/addr`; 7477 is only the fallback.
    let machine = Machine::new();
    let _daemon = Daemon::start(&machine);
    std::fs::write(machine.data_dir().join("addr"), &machine.addr).unwrap();
    machine.run_in(
        &machine.project_root(),
        &["ingest", "prompt", "--host", "test", "--session", "s"],
        b"build a login page",
        false,
    );
    assert_ne!(
        machine.active_run(),
        serde_json::Value::Null,
        "the prompt did not reach the daemon in addr"
    );
}

#[test]
#[ignore = "audit P0.b"]
fn audit_p0_b_client_sends_nothing_to_a_daemon_that_cannot_prove_the_token() {
    // V0.2 W4: the client checks HMAC(token, nonce) from /v1/health before sending a body.
    let machine = Machine::new();
    let daemon = RefusingDaemon::behaving(&machine, stand_in(false, "200 OK", Duration::ZERO));
    let text = "add a billing page with invoices";
    machine.prompt(text);
    let sent: Vec<String> = daemon.received().into_iter().map(|r| r.line).collect();
    assert!(
        sent.is_empty(),
        "the client sent a body to a daemon for another data dir: {sent:?}"
    );
    assert!(
        !machine.spooled_containing(text).is_empty(),
        "the prompt must be spooled instead"
    );
}

#[test]
fn audit_p0_b_client_never_shows_the_token_to_a_daemon_that_cannot_prove_it() {
    // V0.2 W4 (A1-1): the health probe that asks for the proof must not carry the token,
    // or a process squatting on the port collects it from the probe.
    let machine = Machine::new();
    let daemon = RefusingDaemon::behaving(&machine, stand_in(false, "200 OK", Duration::ZERO));
    machine.prompt("add a billing page with invoices");
    machine.speedrun(&["status"], b"");
    machine.speedrun(&["watch", "--once"], b"");
    let shown = daemon.shown_token();
    assert!(
        shown.is_empty(),
        "the token went to a daemon for another data dir in {shown:?}"
    );
}

#[test]
fn guard_p0_b_client_sends_to_a_daemon_that_proves_the_token() {
    let machine = Machine::new();
    let daemon = RefusingDaemon::behaving(&machine, stand_in(true, "200 OK", Duration::ZERO));
    machine.prompt("add a billing page with invoices");
    assert!(
        daemon.received().iter().any(|r| r.line.contains("/v1/prompts")),
        "the prompt never reached a daemon that holds the token"
    );
}

#[test]
#[ignore = "audit M2"]
fn audit_m2_a_prompt_refused_with_401_is_spooled() {
    // V0.2 M2: the client spools on 401 (after re-reading the token once), 5xx and timeouts.
    let machine = Machine::new();
    let daemon = RefusingDaemon::behaving(&machine, stand_in(true, "401 Unauthorized", Duration::ZERO));
    let text = "add a settings page with dark mode";
    machine.prompt(text);
    assert!(!daemon.received().is_empty(), "the client never sent it");
    assert!(
        !machine.spooled_containing(text).is_empty(),
        "a prompt refused with 401 is lost"
    );
}

#[test]
#[ignore = "audit M2"]
fn audit_m2_a_prompt_answered_with_500_is_spooled() {
    let machine = Machine::new();
    let daemon = RefusingDaemon::behaving(
        &machine,
        stand_in(true, "500 Internal Server Error", Duration::ZERO),
    );
    let text = "add a search box to the header";
    machine.prompt(text);
    assert!(!daemon.received().is_empty(), "the client never sent it");
    assert!(
        !machine.spooled_containing(text).is_empty(),
        "a prompt answered with 500 is lost"
    );
}

#[test]
#[ignore = "audit M8"]
fn audit_m8_a_hook_sends_its_own_prompt_first_and_flushes_within_its_budget() {
    // V0.2 M8: record the fresh event first, then flush what fits in about 3 s.
    let machine = Machine::new();
    let spool = machine.data_dir().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let backlog: String = (0..10)
        .map(|i| {
            let entry = serde_json::json!({
                "path": format!("/v1/projects/{PROJECT}/events"),
                "body": {"event_id": format!("backlog-{i}"), "occurred_at": null,
                         "payload": {"type": "agent_turn_ended", "session_id": "s"}},
            });
            format!("{entry}\n")
        })
        .collect();
    std::fs::write(spool.join("pending.jsonl"), backlog).unwrap();
    // Each backlog delivery takes the daemon a second: ten seconds for all of them.
    let daemon = RefusingDaemon::behaving(&machine, stand_in(true, "200 OK", Duration::from_secs(1)));

    let started = Instant::now();
    machine.prompt("add password reset by email");
    let took = started.elapsed();

    let received = daemon.received();
    let lines: Vec<&str> = received.iter().map(|r| r.line.as_str()).collect();
    let prompt = lines.iter().position(|l| l.contains("/v1/prompts"));
    let first_backlog = lines.iter().position(|l| l.contains("/v1/projects/"));
    assert!(prompt.is_some(), "the fresh prompt was never sent: {lines:?}");
    assert!(
        first_backlog.is_none_or(|b| prompt < Some(b)),
        "the backlog went before the fresh prompt: {lines:?}"
    );
    assert!(
        took < Duration::from_secs(6),
        "the hook took {took:?} with a slow backlog"
    );
    for i in 0..10 {
        let id = format!("backlog-{i}");
        let delivered = received.iter().any(|r| contains(&r.body, &id));
        assert!(
            delivered || !machine.spooled_containing(&id).is_empty(),
            "{id} was neither delivered nor kept in the spool"
        );
    }
}

#[test]
#[ignore = "audit W4"]
fn audit_w4_doctor_reports_spooled_and_rejected_counts() {
    // V0.2 W4: `speedrun doctor` prints spool and rejected counts, daemon or not.
    let machine = Machine::new();
    let spool = machine.data_dir().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    std::fs::write(spool.join("pending.jsonl"), "{}\n{}\n{}\n").unwrap();
    std::fs::write(spool.join("rejected.jsonl"), "{}\n{}\n").unwrap();
    let (out, stdout) = machine.doctor();
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(has_line(&stdout, "spooled: 3"), "{stdout}");
    assert!(has_line(&stdout, "rejected: 2"), "{stdout}");
}

#[test]
#[ignore = "audit M7.c"]
fn audit_m7_c_a_failed_spool_write_is_counted_and_doctor_shows_it() {
    // END_STATE M7.c: spool write failures are counted and `doctor` shows them.
    use std::os::unix::fs::PermissionsExt;
    let machine = Machine::new();
    let spool = machine.data_dir().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o500)).unwrap();
    machine.prompt("add a profile page");
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (_, stdout) = machine.doctor();
    assert!(has_line(&stdout, "spool write failures: 1"), "{stdout}");
}

#[test]
#[ignore = "audit W4.a"]
fn audit_w4_a_doctor_explains_a_daemon_serving_another_data_dir() {
    // V0.2 W4: a hook run with a mismatched data dir spools and `doctor` explains why.
    let machine = Machine::new();
    let _daemon = RefusingDaemon::behaving(&machine, stand_in(false, "200 OK", Duration::ZERO));
    let (_, stdout) = machine.doctor();
    assert!(stdout.contains("another data directory"), "{stdout}");
}

#[test]
#[ignore = "audit W6.b"]
fn audit_w6_b_an_untrusted_project_records_nothing_until_init_trust() {
    // V0.2 W6: a cloned repo with project.json records nothing until `speedrun init --trust`.
    let machine = Machine::untrusted();
    let _daemon = Daemon::start(&machine);
    let text = "build a todo app with tags";
    let out = machine.prompt(text);
    assert!(
        out.status.success(),
        "a hook must not fail in an untrusted project"
    );
    assert_eq!(
        machine.active_run(),
        serde_json::Value::Null,
        "an untrusted project recorded a prompt"
    );
    assert!(
        machine.files_containing(text).is_empty(),
        "an untrusted project stored the prompt"
    );

    let trusted = machine.speedrun(&["init", "--trust"], b"");
    assert!(
        trusted.status.success(),
        "{}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    machine.prompt(text);
    assert_ne!(
        machine.active_run(),
        serde_json::Value::Null,
        "the trusted project still records nothing"
    );
}

#[test]
fn guard_w6_b_a_project_created_by_init_is_trusted() {
    let machine = Machine::untrusted();
    let _daemon = Daemon::start(&machine);
    let fresh = machine.root.path().join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    let init = machine.run_in(&fresh, &["init"], b"", true);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));
    machine.run_in(
        &fresh,
        &["ingest", "prompt", "--host", "test", "--session", "s"],
        b"start",
        true,
    );
    let status = machine.run_in(&fresh, &["status", "--json"], b"", true);
    let run: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap_or_default();
    assert!(
        run["run_id"].is_string(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

/// `speedrun status` in `cwd` with `home` as HOME; its stderr.
fn status_stderr(machine: &Machine, cwd: &Path, home: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_speedrun"))
        .args(["status"])
        .current_dir(cwd)
        .env("SPEEDRUN_HOME", machine.data_dir())
        .env("SPEEDRUN_ADDR", &machine.addr)
        .env("SPEEDRUN_NO_AUTOSTART", "1")
        .env("HOME", home)
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
#[ignore = "audit W6.b"]
fn audit_w6_b_project_search_never_goes_above_home() {
    let machine = Machine::new();
    let home = machine.root.path().join("home");
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(machine.root.path().join(".speedrun")).unwrap();
    std::fs::copy(
        machine.project_root().join(".speedrun/project.json"),
        machine.root.path().join(".speedrun/project.json"),
    )
    .unwrap();
    let stderr = status_stderr(&machine, &work, &home);
    assert!(stderr.contains("not in a speedrun project"), "{stderr}");
}

fn git_init(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(ok.success());
}

#[test]
#[ignore = "audit W6.b"]
fn audit_w6_b_project_search_stops_at_the_git_toplevel() {
    let machine = Machine::new();
    let nested = machine.project_root().join("vendor/lib");
    git_init(&nested);
    let stderr = status_stderr(&machine, &nested, machine.root.path());
    assert!(stderr.contains("not in a speedrun project"), "{stderr}");
}

#[test]
fn guard_w6_b_a_project_at_the_git_toplevel_is_found() {
    let machine = Machine::new();
    git_init(&machine.project_root());
    let src = machine.project_root().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let stderr = status_stderr(&machine, &src, machine.root.path());
    assert!(!stderr.contains("not in a speedrun project"), "{stderr}");
}
