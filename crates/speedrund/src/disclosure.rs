//! A run's disclosure key: the random material that makes a tier 2 export
//! checkable without putting prompt text or the key itself anywhere public.
//!
//! The key is created at run start and kept beside the database. The log
//! records only its sha256 commitment; the key is read back only to compute
//! prompt commitments for an export.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use speedrun_core::sign::SEED_LEN;

use crate::keys;

/// Where a run's disclosure key lives, and how it becomes a commitment.
pub struct Disclosure {
    dir: PathBuf,
}

impl Disclosure {
    /// Keys are siblings of the database file, one file per run.
    pub fn beside(database: &Path) -> Self {
        Self {
            dir: database.with_file_name("disclosure"),
        }
    }

    /// Create the key for a new run; returns the commitment stored in
    /// `RunStarted`. A concurrent writer's key is used instead of replacing it.
    pub fn create(&self, run_id: &str) -> Result<String> {
        fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", self.dir.display()))?;
        let key = keys::create_secret(&self.dir.join(run_id))?;
        Ok(speedrun_core::digest::key_commitment(&key))
    }

    /// Read the key a run started with. Never regenerates it.
    pub fn load(&self, run_id: &str) -> Result<[u8; SEED_LEN]> {
        keys::read_secret(&self.dir.join(run_id))
    }
}
