use std::path::{Path, PathBuf};
use std::process::Command;

use crate::trust;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use speedrun_core::paths::PROJECT_FILE;

/// `.speedrun/project.json`. Commit it: it gives the project a stable
/// identity across clones and machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub project_id: String,
    pub category_id: String,
    /// The category version this project's runs use. Pinned by `speedrun
    /// init` so that a newer binary does not change the rules under a
    /// project; `speedrun init --upgrade-category` moves it to the latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_version: Option<u32>,
    pub created_at: DateTime<Utc>,
}

pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    /// Find the nearest enclosing project. Untracked directories return
    /// `None`: nothing is recorded until `speedrun init` has been run.
    ///
    /// The walk stops after the git toplevel (the first directory holding
    /// `.git`) and never looks above `$HOME`, so a project config committed
    /// in a parent directory cannot silently opt in everything below it.
    pub fn find(start: &Path) -> Result<Option<Project>> {
        let start = start
            .canonicalize()
            .with_context(|| format!("resolving {}", start.display()))?;
        let home = match std::env::var_os("HOME") {
            Some(home) => {
                let home = PathBuf::from(home);
                Some(
                    home.canonicalize()
                        .with_context(|| format!("resolving {}", home.display()))?,
                )
            }
            None => None,
        };
        let mut dir = Some(start.as_path());
        while let Some(current) = dir {
            let file = current.join(PROJECT_FILE);
            if file.is_file() {
                let text = std::fs::read_to_string(&file)?;
                let config =
                    serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
                return Ok(Some(Project {
                    root: current.to_path_buf(),
                    config,
                }));
            }
            if home.as_deref() == Some(current) {
                break;
            }
            if current.join(".git").exists() {
                break;
            }
            dir = current.parent();
        }
        Ok(None)
    }

    pub fn require(start: &Path) -> Result<Project> {
        Self::find(start)?.ok_or_else(|| {
            anyhow::anyhow!("not in a speedrun project (no {PROJECT_FILE} found); run `speedrun init`")
        })
    }

    /// Whether this project is listed, with this root, in the data dir's
    /// allowlist. Nothing is recorded for an unlisted project.
    pub fn is_trusted(&self) -> Result<bool> {
        trust::is_trusted(&self.config.project_id, &self.root)
    }

    /// Refuse to record for a project the user has not trusted.
    pub fn require_trusted(&self) -> Result<()> {
        if self.is_trusted()? {
            Ok(())
        } else {
            bail!(
                "{} is not a trusted project for this data directory; run `speedrun init --trust`",
                self.root.display()
            )
        }
    }

    /// Add this project's id and canonical root to the local allowlist.
    pub fn trust(&self) -> Result<()> {
        trust::allow(&self.config.project_id, &self.root)
    }

    pub fn init(start: &Path, category_id: &str, force: bool) -> Result<Project> {
        let root =
            git(start, &["rev-parse", "--show-toplevel"]).map_or_else(|| start.to_path_buf(), PathBuf::from);
        let file = root.join(PROJECT_FILE);
        if file.exists() && !force {
            bail!("{} already exists (use --force to replace it)", file.display());
        }
        let config = ProjectConfig {
            project_id: speedrun_core::new_id(),
            category_id: category_id.to_string(),
            category_version: None,
            created_at: Utc::now(),
        };
        std::fs::create_dir_all(file.parent().expect("has parent"))?;
        std::fs::write(&file, serde_json::to_string_pretty(&config)? + "\n")?;
        let project = Project { root, config };
        // `speedrun init` opts this project in for this data directory.
        project.trust()?;
        Ok(project)
    }

    pub fn resolve_commit(&self, rev: &str) -> Result<String> {
        git(
            &self.root,
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        )
        .with_context(|| format!("{rev} is not a commit in {}", self.root.display()))
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
