use std::process::Command;

#[test]
fn readiness_is_read_only_and_distinguishes_project_trust() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_speedrun"))
            .args(args)
            .current_dir(&project)
            .env("HOME", &home)
            .env("SPEEDRUN_HOME", &data)
            .env("SPEEDRUN_ADDR", "127.0.0.1:1")
            .output()
            .unwrap()
    };
    let output = run(&["readiness", "--json"]);
    assert!(output.status.success());
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["project"], "untracked");
    assert_eq!(state["daemon"], "offline");
    assert_eq!(state["recording"], "unknown");
    assert!(
        !data.exists(),
        "readiness must not start or initialize the daemon"
    );
    assert!(run(&["init"]).status.success());
    let output = run(&["readiness", "--json"]);
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["project"], "trusted");
    assert_eq!(state["recording"], "unknown");
    std::fs::remove_file(data.join("trusted-projects.json")).unwrap();
    let output = run(&["readiness", "--json"]);
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["project"], "untrusted");
}

#[test]
fn readiness_reports_capture_offline_queue_rejections_and_query_failure() {
    use std::io::Write;
    use std::process::Stdio;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let data = temp.path().join("data");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    struct DaemonCleanup {
        home: std::path::PathBuf,
        data: std::path::PathBuf,
        project: std::path::PathBuf,
        addr: String,
    }
    impl Drop for DaemonCleanup {
        fn drop(&mut self) {
            let _ = Command::new(env!("CARGO_BIN_EXE_speedrun"))
                .args(["daemon", "stop"])
                .current_dir(&self.project)
                .env("HOME", &self.home)
                .env("SPEEDRUN_HOME", &self.data)
                .env("SPEEDRUN_ADDR", &self.addr)
                .env("SPEEDRUN_NO_AUTOSTART", "1")
                .output();
        }
    }
    let _cleanup = DaemonCleanup {
        home: home.clone(),
        data: data.clone(),
        project: project.clone(),
        addr: addr.clone(),
    };
    let command = |args: &[&str], offline: bool, input: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_speedrun"));
        cmd.args(args)
            .current_dir(&project)
            .env("HOME", &home)
            .env("SPEEDRUN_HOME", &data)
            .env("SPEEDRUN_ADDR", &addr)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if offline {
            cmd.env("SPEEDRUN_NO_AUTOSTART", "1");
        }
        let mut child = cmd.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    assert!(command(&["init"], false, "").status.success());
    assert!(
        command(
            &["ingest", "prompt", "--host", "fixture", "--session", "s"],
            false,
            "secret fixture prompt"
        )
        .status
        .success()
    );
    let output = command(&["readiness", "--json"], false, "");
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["recording"], "captured");
    assert_eq!(state["last_host"], "fixture");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("secret fixture prompt"));
    assert!(command(&["reset", "--yes"], false, "").status.success());
    let output = command(&["readiness", "--json"], false, "");
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["recording"], "awaiting_prompt");
    assert!(
        command(
            &["ingest", "prompt", "--host", "new-host", "--session", "s2"],
            false,
            "new prompt"
        )
        .status
        .success()
    );
    assert!(command(&["split", "scaffold-runs"], false, "").status.success());
    assert!(command(&["split", "happy-path-demo"], false, "").status.success());
    let status = command(&["status", "--json"], false, "");
    let run: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(run["status"], "completed");
    let output = command(&["readiness", "--json"], false, "");
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["recording"], "captured");
    assert_eq!(state["last_host"], "new-host");
    let db = rusqlite::Connection::open(data.join("speedrun.sqlite")).unwrap();
    db.execute_batch(
        "DROP TRIGGER events_no_update; UPDATE events SET body='invalid' WHERE type='prompt_submitted';",
    )
    .unwrap();
    let output = command(&["readiness", "--json"], false, "");
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["recording"], "unknown");
    assert!(command(&["daemon", "stop"], false, "").status.success());
    command(
        &["ingest", "prompt", "--host", "fixture", "--session", "s"],
        true,
        "queued secret",
    );
    let outbox = rusqlite::Connection::open(data.join("spool/pending.sqlite3")).unwrap();
    outbox.execute("UPDATE outbox SET state='rejected'", []).unwrap();
    command(
        &["ingest", "prompt", "--host", "fixture", "--session", "s"],
        true,
        "another queued secret",
    );
    let output = command(&["readiness", "--json"], true, "");
    let state: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(state["daemon"], "offline");
    assert_eq!(state["recording"], "unknown");
    assert_eq!(state["queue_scope"], "device");
    assert_eq!(state["queued"], 1);
    assert_eq!(state["rejected"], 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("queued secret"));
}

#[test]
fn external_selections_are_not_reported_as_missing() {
    for host in ["claude", "zed"] {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let command = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_speedrun"))
                .args(args)
                .current_dir(&project)
                .env("HOME", &home)
                .env("SPEEDRUN_HOME", temp.path().join("data"))
                .env("SPEEDRUN_ADDR", "127.0.0.1:1")
                .output()
                .unwrap()
        };
        assert!(
            command(&["setup", "--host", host, "--capture", "metadata-only", "--yes"])
                .status
                .success()
        );
        let state: serde_json::Value =
            serde_json::from_slice(&command(&["readiness", "--json"]).stdout).unwrap();
        assert_eq!(state["integration_state"], "external", "{host}");
    }
}
