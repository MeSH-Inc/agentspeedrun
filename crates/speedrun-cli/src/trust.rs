//! The local allowlist behind V0.2 W6: a project is recorded only after
//! this data directory's user trusts its id and canonical root.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use speedrun_core::paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustedProject {
    project_id: String,
    root: PathBuf,
}

fn allowlist_path() -> PathBuf {
    paths::data_dir().join("trusted-projects.json")
}

/// The projects this data directory trusts. A missing file trusts none.
fn load() -> Result<Vec<TrustedProject>> {
    let path = allowlist_path();
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether `project_id` at `root` is trusted. Roots compare canonical.
pub fn is_trusted(project_id: &str, root: &Path) -> Result<bool> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let trusted = load()?;
    Ok(trusted
        .iter()
        .any(|p| p.project_id == project_id && p.root == root))
}

/// How many times `allow` rewrites the allowlist before giving up on a
/// concurrent writer that keeps replacing it.
const ALLOW_ATTEMPTS: usize = 5;

/// Trust `project_id` at `root`, once.
///
/// The list is replaced by renaming a complete temp file over it, so a reader
/// never sees a partial list. Two concurrent writers can still each replace
/// the other's version, so the list is read back and the write repeated
/// until it holds this project.
pub fn allow(project_id: &str, root: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let path = allowlist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut writes = 0;
    loop {
        let mut trusted = load()?;
        if trusted
            .iter()
            .any(|p| p.project_id == project_id && p.root == root)
        {
            return Ok(());
        }
        if writes == ALLOW_ATTEMPTS {
            anyhow::bail!(
                "{} kept changing while {} was added; try again",
                path.display(),
                root.display()
            );
        }
        trusted.push(TrustedProject {
            project_id: project_id.to_string(),
            root: root.clone(),
        });
        let temp = path.with_file_name(format!(".trusted-projects-{}.tmp", speedrun_core::new_id()));
        std::fs::write(&temp, serde_json::to_vec_pretty(&trusted)?)
            .with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, &path).with_context(|| format!("replacing {}", path.display()))?;
        writes += 1;
    }
}
