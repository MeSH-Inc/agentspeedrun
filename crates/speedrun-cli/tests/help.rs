use std::process::Command;

#[test]
fn host_adapter_help_is_printed_successfully() {
    for args in [
        &["ingest", "--help"][..],
        &["ingest", "prompt", "--help"],
        &["hook", "--help"],
        &["hook", "copilot", "--help"],
        &["hook", "copilot", "user-prompt-submit", "--help"],
        &["ingest", "prompt", "-h"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_speedrun"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Usage: speedrun"), "{args:?}: {stdout}");
        assert!(stdout.contains(args[0]), "{args:?}: {stdout}");
        assert!(!stdout.contains("unusable arguments"), "{args:?}: {stdout}");
    }
}

#[test]
fn malformed_ingest_arguments_still_return_successful_json() {
    let output = Command::new(env!("CARGO_BIN_EXE_speedrun"))
        .args(["ingest", "prompt", "--newer-flag"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["recorded"], false);
    assert!(reply["error"].as_str().unwrap().contains("unusable arguments"));
}
