//! The device key: one ed25519 keypair per data directory, generated on first
//! start. It signs every event and its fingerprint is the device id.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use speedrun_core::sign::{SEED_LEN, generate, parse_seed, seed_hex};

/// File name of the prompt key beside the database in the data directory.
pub const PROMPT_KEY_FILE: &str = "prompt.key";

/// File name of the device key beside the database in the data directory.
pub const DEVICE_KEY_FILE: &str = "device.key";

/// One certified step from a previous device key to the next. The
/// certificate is the previous key's signature over the new public key, so a
/// chain that predates the switch still verifies from public keys alone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rotation {
    pub old_pubkey: String,
    pub new_pubkey: String,
    pub certificate: String,
}

/// Read the device key, creating it with mode 0600 if it does not exist.
///
/// A missing key is a first start; an unreadable one is an error, never a
/// silent regeneration, because a new key breaks every signature already made.
pub fn load_or_create(path: &Path) -> Result<SigningKey> {
    load_or_create_seed(path, "to generate a new device key")
}

/// Read a 32-byte secret stored as hex with mode 0600, creating it if absent.
///
/// The prompt key is one: a silent regeneration would make every prompt
/// digest already in a log uncheckable.
pub fn load_or_create_secret(path: &Path) -> Result<[u8; SEED_LEN]> {
    let key = load_or_create_seed(path, "to regenerate the prompt key")?;
    Ok(key.to_bytes())
}

/// Create a fresh 32-byte secret at `path`: hex in a new 0600 file.
pub fn create_secret(path: &Path) -> Result<[u8; SEED_LEN]> {
    Ok(create(path)?.to_bytes())
}

/// Read a 32-byte secret stored as hex. Unlike [`load_or_create_secret`], a
/// missing file is an error: a run's disclosure key must never be regenerated.
pub fn read_secret(path: &Path) -> Result<[u8; SEED_LEN]> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_seed(&text)
        .map(|key| key.to_bytes())
        .map_err(|e| anyhow!("{e}; expected a disclosure key at {}", path.display()))
}

/// The one definition of how a key is kept at rest: hex in a 0600 file,
/// created once and never silently replaced.
fn load_or_create_seed(path: &Path, advice: &str) -> Result<SigningKey> {
    match fs::read_to_string(path) {
        Ok(text) => parse_seed(&text).map_err(|e| anyhow!("{e}; delete {} {advice}", path.display())),
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

/// Replace the device key at `path` atomically: write a fresh 0600 file in
/// the same directory, then rename it over the old one. The previous key is
/// never written again.
pub fn replace(path: &Path, key: &SigningKey) -> Result<()> {
    let temp = path.with_file_name(format!(".device-key-{}.tmp", speedrun_core::new_id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(seed_hex(key).as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .with_context(|| format!("writing {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
