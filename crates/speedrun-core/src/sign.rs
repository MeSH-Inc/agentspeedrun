//! Device keys and event signatures.
//!
//! Every event is signed by the device that produced it. The signature covers
//! the event's `hash`, which in turn covers the canonical body, so a verifier
//! needs nothing but the device's public key. The private key never leaves the
//! device; `RunStarted.device_pubkey` is how a log names its signer.
//!
//! This module has no I/O: key files are the daemon's business.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::{Event, EventPayload};

/// Length of the secret seed, in bytes.
pub const SEED_LEN: usize = 32;
/// Length of a hex-encoded public key (64 hex characters).
pub const PUBKEY_HEX_LEN: usize = 64;

/// A fresh device key from the OS CSPRNG.
pub fn generate() -> SigningKey {
    SigningKey::generate(&mut rand_core::OsRng)
}

/// Parse a hex seed read from the device key file.
pub fn parse_seed(hex_seed: &str) -> Result<SigningKey, String> {
    let bytes = hex::decode(hex_seed.trim()).map_err(|e| format!("device key is not hex: {e}"))?;
    let seed: [u8; SEED_LEN] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("device key must be {SEED_LEN} bytes, found {}", bytes.len()))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn seed_hex(key: &SigningKey) -> String {
    hex::encode(key.to_bytes())
}

pub fn pubkey_hex(key: &SigningKey) -> String {
    hex::encode(key.verifying_key().to_bytes())
}

/// Human-comparable device id: sha256 of the public key, hex.
pub fn fingerprint(pubkey_hex: &str) -> Result<String, String> {
    let key = decode_pubkey(pubkey_hex)?;
    Ok(hex::encode(Sha256::digest(key.as_bytes())))
}

/// Sign an event hash (hex). The result is the hex signature.
pub fn sign_hash(key: &SigningKey, hash_hex: &str) -> String {
    hex::encode(key.sign(hash_hex.as_bytes()).to_bytes())
}

/// Check `sig_hex` over `hash_hex` against a hex public key. Malformed input
/// is not an error: it simply does not verify.
pub fn verify_hash(pubkey_hex: &str, hash_hex: &str, sig_hex: &str) -> bool {
    let Ok(key) = decode_pubkey(pubkey_hex) else {
        return false;
    };
    let Ok(bytes) = hex::decode(sig_hex) else {
        return false;
    };
    let Ok(bytes): Result<[u8; 64], _> = bytes.as_slice().try_into() else {
        return false;
    };
    key.verify_strict(hash_hex.as_bytes(), &Signature::from_bytes(&bytes))
        .is_ok()
}

fn decode_pubkey(pubkey_hex: &str) -> Result<VerifyingKey, String> {
    let s = pubkey_hex.trim();
    if s.len() != PUBKEY_HEX_LEN {
        return Err(format!(
            "device public key must be {PUBKEY_HEX_LEN} hex characters, found {}",
            s.len()
        ));
    }
    let bytes = hex::decode(s).map_err(|e| format!("device public key is not hex: {e}"))?;
    let bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| "device public key is not 32 bytes".to_string())?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| format!("device public key is invalid: {e}"))
}

/// Outcome of checking a run's signatures.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SignatureCheck {
    pub signed: u64,
    /// Events with no signature: schema v1 logs, or a chain that predates
    /// signing.
    pub unsigned: u64,
    /// First signature problem, if any.
    pub error: Option<String>,
}

impl SignatureCheck {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Check every signature in one run's chain against the key the run names.
///
/// A run with no `device_pubkey` (a v1 log) is reported as unsigned rather
/// than broken. A signed event that does not verify is an error.
pub fn verify_event_signatures(events: &[Event]) -> SignatureCheck {
    let mut check = SignatureCheck::default();
    let key = events.iter().find_map(|e| match &e.payload {
        EventPayload::RunStarted {
            device_pubkey: Some(k),
            ..
        } => Some(k.clone()),
        _ => None,
    });
    for e in events {
        match (&e.sig, &key) {
            (Some(sig), Some(k)) => {
                if verify_hash(k, &e.hash, sig) {
                    check.signed += 1;
                } else if check.error.is_none() {
                    check.error = Some(format!("event seq {}: signature does not match", e.seq));
                }
            }
            (Some(_), None) => {
                if check.error.is_none() {
                    check.error = Some(format!(
                        "event seq {}: signed, but the run names no device key",
                        e.seq
                    ));
                }
            }
            (None, _) => check.unsigned += 1,
        }
    }
    check
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SCHEMA_VERSION;
    use chrono::Utc;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; SEED_LEN])
    }

    fn signed(run_key: &SigningKey, seq: u64) -> Event {
        let mut e = Event {
            event_id: format!("e{seq}"),
            schema_version: SCHEMA_VERSION,
            run_id: "r1".into(),
            seq,
            device_id: "d1".into(),
            occurred_at: Utc::now(),
            recorded_at: Utc::now(),
            payload: if seq == 1 {
                EventPayload::RunStarted {
                    project_id: "p".into(),
                    project_root: None,
                    category_id: "any-percent".into(),
                    category_version: 1,
                    device_pubkey: Some(pubkey_hex(run_key)),
                }
            } else {
                EventPayload::AgentTurnEnded {
                    session_id: "s".into(),
                }
            },
            prev_hash: None,
            hash: String::new(),
            sig: None,
        };
        e.hash = crate::hash::compute_hash(&e);
        e.sig = Some(sign_hash(run_key, &e.hash));
        e
    }

    #[test]
    fn seed_roundtrips() {
        let k = key();
        assert_eq!(seed_hex(&parse_seed(&seed_hex(&k)).unwrap()), seed_hex(&k));
        assert_eq!(pubkey_hex(&k).len(), PUBKEY_HEX_LEN);
    }

    #[test]
    fn signature_covers_the_hash() {
        let k = key();
        let h = "ab".repeat(32);
        let sig = sign_hash(&k, &h);
        assert!(verify_hash(&pubkey_hex(&k), &h, &sig));
        assert!(!verify_hash(&pubkey_hex(&k), &"cd".repeat(32), &sig));
        assert!(!verify_hash(&"00".repeat(32), &h, &sig));
        assert!(!verify_hash("not hex", &h, &sig));
    }

    #[test]
    fn signed_run_checks_clean() {
        let k = key();
        let check = verify_event_signatures(&[signed(&k, 1), signed(&k, 2)]);
        assert_eq!(
            check,
            SignatureCheck {
                signed: 2,
                unsigned: 0,
                error: None
            }
        );
    }

    #[test]
    fn tampered_hash_fails_the_signature() {
        let k = key();
        let mut events = vec![signed(&k, 1), signed(&k, 2)];
        events[1].hash = "00".repeat(32);
        let check = verify_event_signatures(&events);
        assert!(check.error.unwrap().contains("seq 2"));
    }

    #[test]
    fn signing_does_not_change_the_hash() {
        let k = key();
        let mut e = signed(&k, 1);
        let h = e.hash.clone();
        e.sig = Some("ff".repeat(64));
        assert_eq!(crate::hash::compute_hash(&e), h);
    }
}
