//! `speedrun doctor`: a side-effect-free diagnosis of the local daemon and
//! spool. It never starts the daemon and never sends the token.

use std::path::Path;

use anyhow::{Context, Result};
use speedrun_core::{api::HealthResponse, paths};

use crate::client::{self, CallError, Client};

/// How many log lines to show. The file can be long; the tail is the part
/// that explains the latest failure.
const LOG_TAIL_LINES: usize = 20;

pub fn print(client: &Client) -> Result<()> {
    println!("cli: {}", env!("CARGO_PKG_VERSION"));
    let addr = client::resolve_addr();
    match client.verify_health() {
        Ok(health) => println!("{}", daemon_line(&health)),
        Err(e) => println!("{}", daemon_failure(&e, &addr)),
    }
    let data_dir = paths::data_dir();
    println!("data dir: {}", data_dir.display());
    let stats = client::spool_stats().context("counting the spool")?;
    println!("spooled: {}", stats.spooled);
    println!("rejected: {}", stats.rejected);
    println!("spool write failures: {}", stats.write_failures);
    print_log_tail(&data_dir)
}

/// The `daemon:` line doctor prints for a daemon that proved this data
/// dir's token. `daemon status` prints it and adds the pid and address.
pub(crate) fn daemon_line(health: &HealthResponse) -> String {
    format!(
        "daemon: {} (protocol {})",
        health.version, health.protocol_version
    )
}

/// The `daemon:` line doctor prints when a probe did not prove the token.
/// `daemon status`, `stop` and `restart` share it, so its wording lives once.
pub(crate) fn daemon_failure(error: &CallError, addr: &str) -> String {
    match error {
        CallError::WrongDataDir { .. } => {
            format!("daemon: answers for another data directory ({addr})")
        }
        CallError::Unreachable(_) | CallError::Api { .. } | CallError::DeliveryUnknown(_) => {
            format!("daemon: not reachable at {addr}")
        }
    }
}

fn print_log_tail(data_dir: &Path) -> Result<()> {
    let path = data_dir.join("speedrund.log");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    for line in &lines[start..] {
        println!("{line}");
    }
    Ok(())
}
