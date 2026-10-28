use std::path::{Path, PathBuf};
use std::process::Command;

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
    pub created_at: DateTime<Utc>,
}

pub struct Project {
    pub root: PathBuf,
    pub config: ProjectConfig,
}

impl Project {
    /// Find the nearest enclosing project. Untracked directories return
    /// `None`: nothing is recorded until `speedrun init` has been run.
    pub fn find(start: &Path) -> Result<Option<Project>> {
        for dir in start.ancestors() {
            let file = dir.join(PROJECT_FILE);
            if file.is_file() {
                let text = std::fs::read_to_string(&file)?;
                let config =
                    serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
                return Ok(Some(Project {
                    root: dir.to_path_buf(),
                    config,
                }));
            }
        }
        Ok(None)
    }

    pub fn require(start: &Path) -> Result<Project> {
        Self::find(start)?.ok_or_else(|| {
            anyhow::anyhow!("not in a speedrun project (no {PROJECT_FILE} found); run `speedrun init`")
        })
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
            created_at: Utc::now(),
        };
        std::fs::create_dir_all(file.parent().expect("has parent"))?;
        std::fs::write(&file, serde_json::to_string_pretty(&config)? + "\n")?;
        Ok(Project { root, config })
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
