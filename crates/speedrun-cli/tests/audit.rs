//! Regression tests for v0.1.0 audit findings (docs/audit/v0.1.0.json, docs/V0.2.md).
//!
//! Tests marked `#[ignore = "audit ..."]` fail until their finding is fixed; run them with
//! `cargo test -- --ignored audit_`. The `guard_` tests pass today and must keep passing.

use std::process::{Command, Stdio};

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
#[ignore = "audit A4-1"]
fn audit_a4_1_hook_with_unknown_arguments_exits_zero() {
    assert_eq!(exit_code(&["hook", "x", "y", "--bogus"]), Some(0));
}

#[test]
#[ignore = "audit A4-1"]
fn audit_a4_1_hook_with_a_flag_from_a_newer_plugin_exits_zero() {
    assert_eq!(
        exit_code(&["hook", "claude-code", "user-prompt-submit", "--added-later"]),
        Some(0)
    );
}

#[test]
#[ignore = "audit A4-1"]
fn audit_a4_1_hook_with_missing_arguments_exits_zero() {
    assert_eq!(exit_code(&["hook", "claude-code"]), Some(0));
}

#[test]
#[ignore = "audit A4-1"]
fn audit_a4_1_ingest_with_bad_arguments_exits_zero() {
    assert_eq!(exit_code(&["ingest", "prompt", "--bogus"]), Some(0));
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
