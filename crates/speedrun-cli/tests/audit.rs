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

impl Machine {
    fn new() -> Self {
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

    fn speedrun(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_speedrun"))
            .args(args)
            .current_dir(self.root.path().join("project"))
            .env("SPEEDRUN_HOME", self.data_dir())
            .env("SPEEDRUN_ADDR", &self.addr)
            .env("SPEEDRUN_NO_AUTOSTART", "1")
            .env("HOME", self.root.path())
            .env_remove("XDG_DATA_HOME")
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

/// A stand-in daemon on a machine's address: healthy, but it refuses every other request
/// with 400, as speedrund refuses an event its run no longer accepts. It keeps the body
/// of every request it refused.
struct RefusingDaemon {
    refused: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl RefusingDaemon {
    fn start(machine: &Machine) -> Self {
        let listener = TcpListener::bind(&machine.addr).unwrap();
        let refused = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&refused);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer(stream, &record);
            }
        });
        Self { refused }
    }

    fn refused(&self) -> Vec<Vec<u8>> {
        self.refused.lock().unwrap().clone()
    }
}

fn answer(stream: TcpStream, refused: &Mutex<Vec<Vec<u8>>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let (mut length, mut chunked) = (0, false);
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
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
    let (status, reply) = if request_line.starts_with("GET /v1/health ") {
        let health = format!(r#"{{"ok":true,"version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
        ("200 OK", health)
    } else {
        refused.lock().unwrap().push(body);
        let error = r#"{"error":"refused by the stand-in daemon"}"#.to_string();
        ("400 Bad Request", error)
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

const PROJECT: &str = "01KAUDITPROJECT0000000000";

/// POST to the machine's daemon with its token; the status and JSON body.
fn post(machine: &Machine, path: &str, body: &serde_json::Value) -> (u16, serde_json::Value) {
    let result = ureq::post(&format!("http://{}{path}", machine.addr))
        .set("Authorization", "Bearer audit-test-token")
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
        .set("Authorization", "Bearer audit-test-token")
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
        .set("Authorization", "Bearer audit-test-token")
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
#[ignore = "audit D5"]
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
