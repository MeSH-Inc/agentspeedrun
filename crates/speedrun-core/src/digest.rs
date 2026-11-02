//! Keyed digests over prompt text: HMAC-SHA256 (RFC 2104), written here so
//! the domain crate needs no further dependency.
//!
//! Two keys use it. The device's prompt key digests each prompt into the log
//! (`PromptSubmitted.content_digest`) and never leaves the machine. A run's
//! disclosure key makes the tier 2 export's commitments, and can be revealed
//! later so anyone can check text against them.

use sha2::{Digest, Sha256};

/// SHA-256's block size in bytes, the unit HMAC pads keys to.
const BLOCK_LEN: usize = 64;
const IPAD: u8 = 0x36;
const OPAD: u8 = 0x5c;

/// HMAC-SHA256 of `message` under `key`.
///
/// The block-size and padding rules stay inside this module: callers see
/// only "a keyed digest of these bytes".
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let block = key_block(key);
    let mut inner = Sha256::new();
    inner.update(masked(&block, IPAD));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(masked(&block, OPAD));
    outer.update(inner);
    let out = outer.finalize();
    let mut digest = [0u8; 32];
    for (slot, byte) in digest.iter_mut().zip(out.iter()) {
        *slot = *byte;
    }
    digest
}

/// The key padded to one SHA-256 block. A key longer than the block is hashed
/// first, so its length never escapes this function.
fn key_block(key: &[u8]) -> [u8; BLOCK_LEN] {
    let mut block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        let hashed = Sha256::digest(key);
        for (slot, byte) in block.iter_mut().zip(hashed.iter()) {
            *slot = *byte;
        }
    } else {
        for (slot, byte) in block.iter_mut().zip(key.iter()) {
            *slot = *byte;
        }
    }
    block
}

/// XOR every byte of the block with `pad`, producing the inner or outer pad.
fn masked(block: &[u8; BLOCK_LEN], pad: u8) -> [u8; BLOCK_LEN] {
    let mut out = [0u8; BLOCK_LEN];
    for (slot, byte) in out.iter_mut().zip(block.iter()) {
        *slot = *byte ^ pad;
    }
    out
}

/// Hex HMAC-SHA256 of `text` under `key`: the form stored in logs and exports.
pub fn text_digest(key: &[u8], text: &str) -> String {
    hex::encode(hmac_sha256(key, text.as_bytes()))
}

/// Hex SHA-256 of a disclosure key's bytes: what a run records at start so
/// the key can be revealed later without being stored in the log.
pub fn key_commitment(key: &[u8]) -> String {
    hex::encode(Sha256::digest(key))
}

/// What `/v1/health` returns for a nonce: hex HMAC-SHA256 under the data
/// dir's token. Only a daemon holding that token can produce it.
pub fn token_proof(token: &str, nonce: &str) -> String {
    hex::encode(hmac_sha256(token.as_bytes(), nonce.as_bytes()))
}
