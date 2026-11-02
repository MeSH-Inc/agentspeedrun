//! The daemon's side of a GitHub link (COMMONS.md §20): the identity it
//! presents and the one claim it will sign.
//!
//! The format rule, the current-key rule and the claim digest live here.
//! Callers ask for the device key or hand over a claim; they never build the
//! digest or reach for the signing key themselves.

use anyhow::Result;
use speedrun_core::commons::{self, DeviceKey, DeviceSignature, LinkClaim};
use speedrun_core::sign::fingerprint;

use crate::store::Store;

/// Why a link claim was not signed.
pub enum LinkError {
    /// The claim is not one this daemon may sign.
    Refused(&'static str),
    /// The claim has no digest to sign.
    Internal(anyhow::Error),
}

impl From<serde_json::Error> for LinkError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(e.into())
    }
}

/// The identity `/v1/device` reports: the key that signs this device's chains
/// now, and the fingerprint its events use as `device_id`.
pub fn device_key(store: &Store) -> Result<DeviceKey> {
    let device_pubkey = store.device_pubkey();
    let device_id = fingerprint(&device_pubkey).map_err(anyhow::Error::msg)?;
    Ok(DeviceKey {
        device_pubkey,
        device_id,
    })
}

/// Sign `claim` only when it is format 1 and names the key this daemon holds
/// now. No other claim reaches the signing key through this module.
pub fn sign(store: &Store, claim: &LinkClaim) -> Result<DeviceSignature, LinkError> {
    if claim.format != commons::LINK_FORMAT {
        return Err(LinkError::Refused("link claim format is not 1"));
    }
    if claim.device_pubkey != store.device_pubkey() {
        return Err(LinkError::Refused("link claim names a different device key"));
    }
    let digest = commons::link_claim_digest(claim)?;
    Ok(DeviceSignature {
        device_signature: store.sign_digest(&digest),
    })
}
