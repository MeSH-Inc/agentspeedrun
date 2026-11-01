//! The device key: one ed25519 keypair per data directory, generated on first
//! start. It signs every event and its fingerprint is the device id.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::SigningKey;
use speedrun_core::sign::{generate, parse_seed, seed_hex};

/// Read the device key, creating it with mode 0600 if it does not exist.
///
/// A missing key is a first start; an unreadable one is an error, never a
/// silent regeneration, because a new key breaks every signature already made.
pub fn load_or_create(path: &Path) -> Result<SigningKey> {
    match fs::read_to_string(path) {
        Ok(text) => parse_seed(&text)
            .map_err(|e| anyhow!("{e}; delete {} to generate a new device key", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create(path),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn create(path: &Path) -> Result<SigningKey> {
    let key = generate();
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(seed_hex(&key).as_bytes())
                .and_then(|()| f.write_all(b"\n"))
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(key)
        }
        // Another daemon won the race; use what it wrote.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            parse_seed(&text).map_err(|e| anyhow!("{e}; delete {} to regenerate", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}
