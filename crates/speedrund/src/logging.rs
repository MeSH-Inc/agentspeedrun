//! Diagnostics policy: where the daemon writes its logs, how they are
//! filtered, and how a failed verification is reported. Callers say what
//! happened; this module owns whether and how it reaches the user.

use std::net::SocketAddr;
use std::path::Path;

/// Install the global subscriber once at startup. Diagnostics go to stderr
/// because autostart discards stdout. `SPEEDRUN_LOG` is the filter. A missing
/// value means `info`; an unusable one also means `info`, with a warning, so a
/// bad log setting never stops the daemon from starting.
pub fn init() {
    let (filter, problem) = match std::env::var("SPEEDRUN_LOG") {
        Ok(directives) => match tracing_subscriber::EnvFilter::try_new(directives.as_str()) {
            Ok(filter) => (filter, None),
            Err(e) => (
                tracing_subscriber::EnvFilter::new("info"),
                Some(format!(
                    "SPEEDRUN_LOG is not a valid log filter ({e}); using info"
                )),
            ),
        },
        Err(std::env::VarError::NotPresent) => (tracing_subscriber::EnvFilter::new("info"), None),
        Err(std::env::VarError::NotUnicode(_)) => (
            tracing_subscriber::EnvFilter::new("info"),
            Some("SPEEDRUN_LOG is not valid Unicode; using info".to_string()),
        ),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    if let Some(problem) = problem {
        tracing::warn!("{problem}");
    }
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
pub fn verification_failed(outcome: &speedrun_verify::Outcome) {
    if !outcome.verified() {
        tracing::info!(problems = ?outcome.problems, "evidence not verified");
    }
}
