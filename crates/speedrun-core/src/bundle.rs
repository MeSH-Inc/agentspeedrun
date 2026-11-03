//! The publication bundle (COMMONS.md §7): what `speedrun publish` uploads and
//! what the commons stores verbatim. A bundle is reproducible on its own: its
//! events carry their hashes and device signatures, so anyone holding it can
//! check the chain and fold the run without the author's machine.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::api::PromptCommitment;
use crate::hash::canonical_digest;

/// The bundle format this build writes and reads.
pub const BUNDLE_FORMAT: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Bundle {
    /// [`BUNDLE_FORMAT`].
    pub format: u32,
    pub run_id: String,
    /// Disclosure tier (COMMONS.md §5). 0 anchors the run's chain heads and
    /// publishes nothing else; 1 publishes the signed log; 2 adds one
    /// commitment per prompt under the run's disclosure key.
    pub tier: u8,
    /// Tiers 1 and 2: every event of the run, each exactly as the daemon
    /// stored it (`hash` and `sig` included), ordered by `device_id` then
    /// `seq`. Empty at tier 0.
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
    /// Tier 0: the last event of each device chain. Empty at tiers 1 and 2.
    #[serde(default)]
    pub chain_heads: Vec<ChainHead>,
    /// Tier 2: one commitment per prompt, in log order, as `speedrun export`
    /// gives them. Empty at tiers 0 and 1.
    #[serde(default)]
    pub prompts: Vec<PromptCommitment>,
    /// Certificates from the run's origin key to the publishing key, in order.
    #[serde(default)]
    pub rotations: Vec<RotationProof>,
    /// Hex ed25519 public key of the device that signs this bundle. At tiers
    /// 1 and 2 it must be a key that signs one of the run's chains.
    pub publisher_pubkey: String,
    /// Hex signature of `publisher_pubkey` over [`digest`] of this bundle
    /// (`sign::sign_hash`).
    pub signature: String,
}

/// An origin-key rotation, authorized by the previous key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RotationProof {
    pub new_pubkey: String,
    pub certificate: String,
}

/// The origin key is part of the run identifier and cannot be substituted.
pub fn origin_key(run_id: &str) -> Option<&str> {
    let (nonce, key) = run_id.split_once('.')?;
    if nonce.len() != 26
        || key.len() != 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(key)
}

/// The last event of one device's chain in a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChainHead {
    pub device_id: String,
    pub seq: u64,
    pub hash: String,
}

/// A bundle's identity: the canonical digest (`hash::canonical_digest`) of
/// its JSON with `signature` removed. The commons stores and anchors bundles
/// under this digest, and the publisher's signature covers it.
pub fn digest(bundle: &Bundle) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(bundle)?;
    if let serde_json::Value::Object(fields) = &mut value {
        fields.remove("signature");
    }
    Ok(canonical_digest(&value))
}

/// JSON Schema of bundle format 2, committed as `schema/bundle.schema.json`.
pub fn bundle_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Bundle)).expect("schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_bundle_schema_is_current() {
        let committed: serde_json::Value =
            serde_json::from_str(include_str!("../../../schema/bundle.schema.json")).unwrap();
        assert!(
            committed == bundle_schema(),
            "schema/bundle.schema.json is stale; regenerate it with `speedrun schema --bundle`"
        );
    }

    #[test]
    fn the_digest_does_not_cover_the_signature() {
        let mut bundle = Bundle {
            format: BUNDLE_FORMAT,
            run_id: "r".into(),
            tier: 1,
            events: vec![],
            chain_heads: vec![],
            prompts: vec![],
            rotations: vec![],
            publisher_pubkey: "ab".repeat(32),
            signature: String::new(),
        };
        let unsigned = digest(&bundle).unwrap();
        bundle.signature = "cd".repeat(64);
        assert_eq!(digest(&bundle).unwrap(), unsigned);
        bundle.tier = 2;
        assert_ne!(digest(&bundle).unwrap(), unsigned);
    }
}
