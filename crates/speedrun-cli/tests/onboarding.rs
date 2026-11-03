use std::process::Command;

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_speedrun"))
        .current_dir(root)
        .env("HOME", root)
        .env("SPEEDRUN_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn noninteractive_setup_requires_explicit_choices_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let missing = run(root, &["setup"]);
    assert!(!missing.status.success());
    assert!(!root.join(".speedrun/project.json").exists());
    let args = [
        "setup",
        "--host",
        "pi",
        "--scope",
        "project",
        "--category",
        "any-percent",
        "--capture",
        "metadata-only",
        "--yes",
    ];
    let first = run(root, &args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let project = std::fs::read(root.join(".speedrun/project.json")).unwrap();
    assert!(root.join(".pi/extensions/agentspeedrun.ts").exists());
    let prefs: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("data/preferences.json")).unwrap()).unwrap();
    assert_eq!(prefs["capture"], "metadata_only");
    let output = String::from_utf8(first.stdout).unwrap();
    assert!(output.contains("capture has not been verified"));
    assert!(output.contains("Readiness is read-only"));
    assert!(output.contains("`speedrun flush`"));
    assert!(output.contains("`speedrun local-test` requires a clean working tree"));
    assert!(!root.join("data/speedrun.sqlite").exists());
    let second = run(
        root,
        &[
            "setup",
            "--host",
            "pi",
            "--scope",
            "project",
            "--capture",
            "metadata-only",
            "--yes",
        ],
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        std::fs::read(root.join(".speedrun/project.json")).unwrap(),
        project
    );
}

#[test]
fn positional_setup_remains_adapter_only() {
    let dir = tempfile::tempdir().unwrap();
    let result = run(dir.path(), &["setup", "pi", "--project"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(dir.path().join(".pi/extensions/agentspeedrun.ts").exists());
    assert!(!dir.path().join(".speedrun/project.json").exists());
    assert!(!dir.path().join("data/preferences.json").exists());
}

#[test]
fn conflicts_are_rejected_before_preferences_or_project_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join(".opencode/plugins")).unwrap();
    let adapter = root.join(".opencode/plugins/agentspeedrun.ts");
    std::fs::write(&adapter, "foreign plugin").unwrap();
    let result = run(
        root,
        &[
            "setup",
            "--host",
            "opencode",
            "--scope",
            "project",
            "--capture",
            "content",
            "--yes",
        ],
    );
    assert!(!result.status.success());
    assert_eq!(std::fs::read_to_string(adapter).unwrap(), "foreign plugin");
    assert!(!root.join(".speedrun/project.json").exists());
    assert!(!root.join("data/preferences.json").exists());
}

#[test]
fn interactive_setup_asks_and_accepts_terminal_choices() {
    let dir = tempfile::tempdir().unwrap();
    let script = r#"
import os, pty, select, subprocess, sys, time
master, slave = pty.openpty()
env = dict(os.environ, HOME=sys.argv[2], SPEEDRUN_HOME=os.path.join(sys.argv[2], 'data'))
p = subprocess.Popen([sys.argv[1], 'setup'], cwd=sys.argv[2], env=env, stdin=slave, stdout=slave, stderr=slave)
os.close(slave)
os.write(master, b'pi\n\n\nmetadata-only\ny\n')
out = b''
deadline = time.monotonic() + 15
while p.poll() is None and time.monotonic() < deadline:
    if select.select([master], [], [], .1)[0]:
        try: out += os.read(master, 65536)
        except OSError: break
if p.poll() is None:
    p.kill()
    raise SystemExit('setup timed out')
while select.select([master], [], [], .1)[0]:
    try:
        chunk = os.read(master, 65536)
        if not chunk: break
        out += chunk
    except OSError: break
sys.stdout.buffer.write(out)
raise SystemExit(p.wait())
"#;
    let result = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_speedrun"))
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("Adapter scope"));
    assert!(output.contains("Capture (content/metadata-only)"));
    assert!(dir.path().join(".pi/extensions/agentspeedrun.ts").exists());
}
