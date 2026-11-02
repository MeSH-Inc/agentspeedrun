//! Reading a project's git repository. Every git process starts here, and
//! callers ask for facts (the HEAD, a commit's committer time) rather than
//! parsing git's output themselves.
//!
//! Absence is a normal answer: a project root that is not a repository, or a
//! repository with no commits, simply has no starting point to record.

use std::process::Command;

use chrono::{DateTime, Utc};

/// The commit a project's `HEAD` names, and when it was committed.
#[derive(Debug, Clone)]
pub struct Head {
    pub sha: String,
    pub committed_at: DateTime<Utc>,
}

/// Standard output of a successful git invocation in `root`; `None` when git
/// is missing, `root` is not a repository, or the command fails.
pub fn output(root: &str, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// `HEAD` as a `(sha, committer time)` pair, when there is one to record.
pub fn head(root: &str) -> Result<Option<Head>, String> {
    let Some(raw) = output(root, &["rev-parse", "--verify", "HEAD"]) else {
        return Ok(None);
    };
    let sha = raw.trim().to_string();
    if sha.is_empty() {
        return Ok(None);
    }
    let secs = commit_time(root, &sha)?;
    let committed_at = DateTime::from_timestamp(secs, 0)
        .ok_or_else(|| format!("commit {sha} has an unrepresentable committer time"))?;
    Ok(Some(Head { sha, committed_at }))
}

/// The commit's committer time, in whole seconds since the Unix epoch.
pub fn commit_time(root: &str, sha: &str) -> Result<i64, String> {
    let raw = output(root, &["show", "-s", "--format=%ct", sha])
        .ok_or_else(|| format!("commit {sha} has no readable committer time"))?;
    raw.trim()
        .parse()
        .map_err(|_| format!("commit {sha} has no readable committer time"))
}
