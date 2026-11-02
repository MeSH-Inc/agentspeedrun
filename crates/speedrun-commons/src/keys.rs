//! The commons key: one ed25519 keypair per data directory, generated on
//! first start. It signs judgments and tree heads, so it is never silently
//! regenerated: a new key would orphan every signature already issued.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use speedrun_core::sign::{generate, parse_seed, seed_hex};

/// Read the key at `path` (a hex seed), creating it with mode 0600 if absent.
pub fn load_or_create(path: &Path) -> Result<SigningKey> {
    match fs::read_to_string(path) {
        Ok(text) => parse_seed(text.trim())
            .map_err(|e| anyhow::anyhow!("{} is not a commons key: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = generate();
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("creating {}", path.display()))?;
            file.write_all(seed_hex(&key).as_bytes())?;
            file.sync_all()?;
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}
