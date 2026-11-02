//! Keyed digests over prompt text: HMAC-SHA256 (RFC 2104), written here so
//! the domain crate needs no further dependency.
//!
//! Two keys use it. The device's prompt key digests each prompt into the log
//! (`PromptSubmitted.content_digest`) and never leaves the machine. A run's
//! disclosure key makes the tier 2 export's commitments, and can be revealed
//! later so anyone can check text against them.

/// HMAC-SHA256 of `message` under `key`.
///
/// Not implemented yet: returns all zeros.
pub fn hmac_sha256(_key: &[u8], _message: &[u8]) -> [u8; 32] {
    [0u8; 32]
}

/// Hex HMAC-SHA256 of `text` under `key`: the form stored in logs and exports.
pub fn text_digest(key: &[u8], text: &str) -> String {
    hex::encode(hmac_sha256(key, text.as_bytes()))
}
