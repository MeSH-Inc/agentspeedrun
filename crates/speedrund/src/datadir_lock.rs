//! Ownership of one data directory.
//!
//! One design decision lives here: how a running daemon claims a data
//! directory and how a second daemon learns that it lost. Callers hold the
//! guard for the process's life and never learn which kernel primitive
//! provides the claim, which file anchors it, or how long an attempt may
//! take.
//!
//! The claim is a kernel file lock held by a live SQLite connection. The
//! kernel drops it when the process dies, however it dies, so a killed daemon
//! leaves nothing to clean up: the file that anchors the lock is inert, and
//! only the kernel lock says who owns the directory.
//!
//! Two rules keep a lost race short. The claim never sets a busy timeout, so a
//! daemon cannot queue behind a lock the owner holds for its whole life; and
//! it never uses SQLite's exclusive locking mode, whose retained SHARED lock
//! lets two processes block each other's escalation. A failed attempt drops
//! its connection, so no SHARED lock outlives the attempt.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, ErrorCode};

/// The file whose kernel lock says which process owns the data directory.
const LOCK_FILE: &str = "speedrund.lock";

/// The most a second daemon spends discovering it lost. The winner holds the
/// lock for its whole life, so this is a deadline, not a queue position.
const GIVE_UP_AFTER: Duration = Duration::from_millis(250);

/// The pause between attempts inside that deadline. Each attempt is on a
/// fresh connection, so this is a new race, not a retried half-held lock.
const RETRY_EVERY: Duration = Duration::from_millis(3);

/// Proof that this process owns its data directory. Dropping it releases the
/// claim, and so does the process dying; nothing runs on the way out.
pub struct DataDirLock {
    /// The connection whose open write transaction is the claim.
    _claim: Connection,
}

impl DataDirLock {
    /// Claim `data_dir`, or fail within a bounded, short time when another
    /// daemon holds it.
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join(LOCK_FILE);
        let deadline = Instant::now() + GIVE_UP_AFTER;
        loop {
            if let Some(claim) = claim(&path)? {
                return Ok(Self { _claim: claim });
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "another daemon owns {}; {} is locked for that daemon's whole life",
                    data_dir.display(),
                    path.display()
                );
            }
            std::thread::sleep(RETRY_EVERY.min(left));
        }
    }
}

/// One attempt: `Some(connection)` when this process now holds the claim,
/// `None` when another process holds it, and an error only when the lock file
/// itself cannot be used.
fn claim(path: &Path) -> Result<Option<Connection>> {
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::ZERO)
        .with_context(|| format!("configuring {}", path.display()))?;
    match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => Ok(Some(conn)),
        Err(e) if contended(&e) => Ok(None),
        Err(e) => Err(anyhow::Error::new(e)).with_context(|| format!("locking {}", path.display())),
    }
}

/// Whether SQLite refused because another connection holds the lock, rather
/// than because the lock file is unusable.
fn contended(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_claim_on_a_held_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let _held = DataDirLock::acquire(dir.path()).unwrap();
        assert!(DataDirLock::acquire(dir.path()).is_err());
    }

    #[test]
    fn releasing_the_claim_lets_the_next_daemon_take_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).unwrap();
        drop(first);
        assert!(DataDirLock::acquire(dir.path()).is_ok());
    }
}
