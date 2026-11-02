//! `speedrun doctor`: a side-effect-free diagnosis of the local daemon and
//! spool. It never starts the daemon and never sends the token.

use std::path::Path;

use anyhow::{Context, Result};
use speedrun_core::paths;

use crate::client::{self, CallError, Client};

/// How many log lines to show. The file can be long; the tail is the part
/// that explains the latest failure.
const LOG_TAIL_LINES: usize = 20;

pub fn print(client: &Client) -> Result<()> {
    println!("cli: {}", env!("CARGO_PKG_VERSION"));
    let addr = client::resolve_addr();
    match client.verify_health() {
        Ok(health) => println!(
            "daemon: {} (protocol {})",
            health.version, health.protocol_version
        ),
        Err(CallError::WrongDataDir { .. }) => {
            println!("daemon: answers for another data directory ({addr})")
        }
        Err(CallError::Unreachable(_) | CallError::Api { .. }) => {
            println!("daemon: not reachable at {addr}")
        }
    }
    let data_dir = paths::data_dir();
    println!("data dir: {}", data_dir.display());
    let stats = client::spool_stats().context("counting the spool")?;
    println!("spooled: {}", stats.spooled);
    println!("rejected: {}", stats.rejected);
    println!("spool write failures: {}", stats.write_failures);
    print_log_tail(&data_dir)
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
