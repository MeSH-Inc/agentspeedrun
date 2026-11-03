use std::{
    io::Write,
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn terminal_selection_retries_cancels_and_records_a_claim() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial"
            ])
            .current_dir(&project)
            .status()
            .unwrap()
            .success()
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_speedrun"));
    let base = || {
        let mut cmd = Command::new(binary);
        cmd.current_dir(&project)
            .env("HOME", root.path())
            .env("SPEEDRUN_HOME", root.path().join("data"))
            .env("SPEEDRUN_ADDR", &addr)
            .env("SPEEDRUN_NO_AUTOSTART", "1");
        cmd
    };
    let _daemon = Daemon(
        Command::new(binary.with_file_name("speedrund"))
            .env("HOME", root.path())
            .env("SPEEDRUN_HOME", root.path().join("data"))
            .env("SPEEDRUN_ADDR", &addr)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(&addr).is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        base()
            .args(["init", "--category", "hundred-percent"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["add", ".speedrun/project.json"])
            .current_dir(&project)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "configure"
            ])
            .current_dir(&project)
            .status()
            .unwrap()
            .success()
    );
    let mut prompt = base()
        .args(["ingest", "prompt", "--host", "test", "--session", "s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    prompt.stdin.take().unwrap().write_all(b"test").unwrap();
    assert!(prompt.wait().unwrap().success());
    let script = r#"
import os, pty, select, subprocess, sys, time
master, slave = pty.openpty()
p = subprocess.Popen([sys.argv[1], 'milestone', sys.argv[3]], stdin=slave, stdout=slave, stderr=slave)
os.close(slave)
os.write(master, sys.argv[2].encode())
out = b''
deadline = time.monotonic() + 15
while p.poll() is None and time.monotonic() < deadline:
    if select.select([master], [], [], .1)[0]:
        try: out += os.read(master, 65536)
        except OSError: break
if p.poll() is None: p.kill(); raise SystemExit('milestone timed out')
sys.stdout.buffer.write(out)
raise SystemExit(p.wait())
"#;
    let terminal = |milestone: &str, input: &str| {
        base()
            .get_envs()
            .fold(Command::new("python3"), |mut command, (k, v)| {
                if let Some(v) = v {
                    command.env(k, v);
                }
                command
            })
            .current_dir(&project)
            .args(["-c", script, binary.to_str().unwrap(), input, milestone])
            .output()
            .unwrap()
    };
    let cancelled = terminal("scaffold-runs", "0\nq\n");
    assert!(
        cancelled.status.success(),
        "{}",
        String::from_utf8_lossy(&cancelled.stdout)
    );
    assert!(String::from_utf8_lossy(&cancelled.stdout).contains("Enter a number"));
    let state = || {
        serde_json::from_slice::<serde_json::Value>(
            &base()
                .args(["milestone", "scaffold-runs", "--json"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    assert_eq!(state()["milestones"][0]["state"], "accepted");
    let recorded = terminal("scaffold-runs", "1\n");
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stdout)
    );
    assert_eq!(state()["milestones"][0]["counts"], true);

    let empty = terminal("tests-green", "1\n1\n");
    assert!(empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stdout).contains("No local test observations"));
    let test_state = || {
        serde_json::from_slice::<serde_json::Value>(
            &base()
                .args(["milestone", "tests-green", "--json"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    assert_eq!(test_state()["milestones"][0]["state"], "accepted");
    let observation = base()
        .args(["local-test", "--", "sh", "-c", "exit 0 # secret-argument"])
        .output()
        .unwrap();
    assert!(
        observation.status.success(),
        "{}",
        String::from_utf8_lossy(&observation.stderr)
    );
    let cancelled = terminal("tests-green", "1\n1\nq\n");
    let output = String::from_utf8_lossy(&cancelled.stdout);
    assert!(cancelled.status.success(), "{output}");
    assert!(
        output.contains("passed") && output.contains("current HEAD"),
        "{output}"
    );
    assert!(!output.contains("secret-argument"));
    assert_eq!(test_state()["milestones"][0]["state"], "accepted");
    let recorded = terminal("tests-green", "1\n1\n1\n");
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stdout)
    );
    assert_eq!(test_state()["milestones"][0]["state"], "verified");
}
