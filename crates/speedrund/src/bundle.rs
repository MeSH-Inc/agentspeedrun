//! What each disclosure tier publishes, as one plain transformation.
//!
//! The service supplies a run's stored rows, its prompt commitments and the
//! device key; this module decides what of them a bundle carries. Keeping the
//! tier rule here means neither the store nor the HTTP layer encodes one.

use std::collections::BTreeMap;

use speedrun_core::Event;
use speedrun_core::api::PromptCommitment;
use speedrun_core::bundle::{BUNDLE_FORMAT, Bundle, ChainHead, RotationProof, digest};

/// Build the bundle for `tier` from the run's stored rows (each a typed event
/// and the body it was stored as), its prompt commitments and the publisher's
/// public key. `sign` signs the bundle's digest with the publisher's key.
pub fn build<F>(
    tier: u8,
    run_id: &str,
    rows: Vec<(Event, serde_json::Value)>,
    prompts: Vec<PromptCommitment>,
    publisher_pubkey: String,
    sign: F,
) -> Result<Bundle, serde_json::Error>
where
    F: FnOnce(&str) -> String,
{
    let origin_device = rows
        .iter()
        .find_map(|(event, _)| {
            matches!(event.payload, speedrun_core::EventPayload::RunStarted { .. })
                .then_some(event.device_id.as_str())
        })
        .unwrap_or("");
    let rotations = rows
        .iter()
        .filter_map(|(event, _)| {
            if event.device_id != origin_device {
                return None;
            }
            match &event.payload {
                speedrun_core::EventPayload::DeviceRotated {
                    new_pubkey,
                    certificate,
                } => Some(RotationProof {
                    new_pubkey: new_pubkey.clone(),
                    certificate: certificate.clone(),
                }),
                _ => None,
            }
        })
        .collect();
    let (events, chain_heads): (Vec<serde_json::Value>, Vec<ChainHead>) = match tier {
        0 => (Vec::new(), heads(&rows)),
        _ => (rows.into_iter().map(|(_, body)| body).collect(), Vec::new()),
    };
    let mut bundle = Bundle {
        format: BUNDLE_FORMAT,
        run_id: run_id.to_string(),
        tier,
        events,
        chain_heads,
        prompts: if tier == 2 { prompts } else { Vec::new() },
        rotations,
        publisher_pubkey,
        signature: String::new(),
    };
    bundle.signature = sign(&digest(&bundle)?);
    Ok(bundle)
}

/// The last event of each device's chain, one per `device_id`, in device order.
fn heads(rows: &[(Event, serde_json::Value)]) -> Vec<ChainHead> {
    let mut heads: BTreeMap<&str, &Event> = BTreeMap::new();
    for (event, _) in rows {
        match heads.get(event.device_id.as_str()) {
            Some(last) if last.seq >= event.seq => {}
            _ => {
                heads.insert(event.device_id.as_str(), event);
            }
        }
    }
    heads
        .into_iter()
        .map(|(device_id, event)| ChainHead {
            device_id: device_id.to_string(),
            seq: event.seq,
            hash: event.hash.clone(),
        })
        .collect()
}
