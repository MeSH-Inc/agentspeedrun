//! Diagnostics policy: where the daemon writes its logs, how they are
//! filtered, and how a failed verification is reported. Callers say what
//! happened; this module owns whether and how it reaches the user.

use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Result, bail};

/// Install the global subscriber once at startup. Diagnostics go to stderr
/// because autostart discards stdout. `SPEEDRUN_LOG` is the filter; only its
/// absence falls back to `info`, while an unparsable value is an error.
pub fn init() -> Result<()> {
    let filter = match std::env::var("SPEEDRUN_LOG") {
        Ok(directives) => tracing_subscriber::EnvFilter::try_new(directives.as_str())
            .map_err(|e| anyhow::anyhow!("SPEEDRUN_LOG is not a valid log filter: {e}"))?,
        Err(std::env::VarError::NotPresent) => tracing_subscriber::EnvFilter::new("info"),
        Err(std::env::VarError::NotUnicode(_)) => bail!("SPEEDRUN_LOG is not valid Unicode"),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    Ok(())
}

/// The one startup line: which build is running, where it listens, and which
/// data directory it owns.
pub fn startup(addr: SocketAddr, data_dir: &Path, device_pubkey: &str) {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        %addr,
        data_dir = %data_dir.display(),
        device_pubkey = %device_pubkey,
        "speedrund listening"
    );
}

/// A verification that did not verify, with the verifier's reasons. `info`
/// is the level the default filter shows; only problem strings are logged,
/// never prompts, tokens or evidence payloads.
pub fn verification_failed(outcome: &crate::verify::Outcome) {
    if !outcome.verified() {
        tracing::info!(problems = ?outcome.problems, "evidence not verified");
    }
}
