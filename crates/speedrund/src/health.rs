//! What a client asks a daemon before it sends it anything: which data
//! directory it serves, which protocol it speaks, and proof that it holds that
//! data directory's token.
//!
//! The identity rule (the canonical data-dir path, hashed) and the proof rule
//! (delegated to `digest::token_proof`) live here, so the HTTP layer only
//! forwards the nonce.

use std::path::Path;

use anyhow::{Context, Result};
use speedrun_core::{
    api::{HealthResponse, PROTOCOL_VERSION},
    digest,
};

/// The daemon's stable identity: the data directory it owns.
pub struct Health {
    data_dir_id: String,
}

impl Health {
    /// Resolve the data directory to the id `/v1/health` reports. The path is
    /// canonicalized first, so the same directory behind a symlink or a
    /// relative path has one id.
    pub fn resolve(data_dir: &Path) -> Result<Self> {
        let canonical =
            std::fs::canonicalize(data_dir).with_context(|| format!("resolving {}", data_dir.display()))?;
        Ok(Self {
            data_dir_id: speedrun_core::hash::sha256_hex(canonical.as_os_str().as_encoded_bytes()),
        })
    }

    /// Answer a health request. `nonce` makes it a proof request: the response
    /// then proves possession of `token` without revealing it.
    pub fn response(&self, token: &str, nonce: Option<&str>) -> HealthResponse {
        HealthResponse {
            ok: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            data_dir_id: self.data_dir_id.clone(),
            pid: std::process::id(),
            proof: nonce.map(|n| digest::token_proof(token, n)),
        }
    }
}
