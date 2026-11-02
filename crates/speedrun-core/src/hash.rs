use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Event;

/// Serialize a JSON value with object keys sorted recursively, so the hash
/// does not depend on serde_json feature flags or field order.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// sha256 (hex) of a JSON value's canonical form: a stable digest of a JSON
/// document, independent of field order.
pub fn canonical_digest(v: &Value) -> String {
    hex::encode(Sha256::digest(canonical_json(v).as_bytes()))
}

/// sha256 (hex) of raw bytes: the stable identity of something that is not
/// JSON, such as a canonical filesystem path.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A stable id derived from `parts`: their sha256, hex-encoded. Each part is
/// NUL-terminated, so `["ab", "c"]` and `["a", "bc"]` differ. The same parts
/// always give the same id, which lets a retried request name the same thing.
pub fn content_id(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hex::encode(hasher.finalize())
}

/// Hash of an event: sha256 over its canonical JSON with `hash` and `sig`
/// removed. The signature covers this hash, so it cannot be part of it.
pub fn compute_hash(event: &Event) -> String {
    let mut v = serde_json::to_value(event).expect("Event always serializes");
    if let Value::Object(map) = &mut v {
        map.remove("hash");
        map.remove("sig");
    }
    hex::encode(Sha256::digest(canonical_json(&v).as_bytes()))
}

/// Hash of a stored event body: sha256 over the canonical JSON of the body as
/// stored, with `hash` and `sig` removed. Unlike [`compute_hash`], fields this
/// build does not know are kept, so a body written by a newer build still
/// hashes to what that build stored (W1.a).
pub fn compute_stored_hash(body: &Value) -> String {
    let mut body = body.clone();
    if let Value::Object(map) = &mut body {
        map.remove("hash");
        map.remove("sig");
    }
    hex::encode(Sha256::digest(canonical_json(&body).as_bytes()))
}

/// Check that stored event bodies (one run) hold intact chains. Bodies are
/// grouped by `device_id` and each device's chain is checked on its own,
/// hashing each body as stored ([`compute_stored_hash`]) rather than its
/// typed parse. The same checks and errors as [`verify_chain`].
///
/// A body whose `seq`, `run_id` or `hash` is missing or the wrong shape cannot
/// be part of an intact chain, so it is reported as tampered at its position.
pub fn verify_stored_chain(bodies: &[Value]) -> Result<(), ChainError> {
    let expected_run = bodies
        .first()
        .and_then(|body| body.get("run_id"))
        .and_then(Value::as_str);
    for chain in stored_device_chains(bodies) {
        let mut prev_hash: Option<&str> = None;
        for (i, body) in chain.into_iter().enumerate() {
            let expected = i as u64 + 1;
            let seq = body
                .get("seq")
                .and_then(Value::as_u64)
                .ok_or(ChainError::Tampered { seq: expected })?;
            if seq != expected {
                return Err(ChainError::Gap { seq, expected });
            }
            let found_run = body
                .get("run_id")
                .and_then(Value::as_str)
                .ok_or(ChainError::Tampered { seq })?;
            if let Some(expected_run) = expected_run
                && found_run != expected_run
            {
                return Err(ChainError::WrongRun {
                    seq,
                    found: found_run.to_string(),
                    expected: expected_run.to_string(),
                });
            }
            let linked = match body.get("prev_hash") {
                None | Some(Value::Null) => None,
                Some(Value::String(hash)) => Some(hash.as_str()),
                Some(_) => return Err(ChainError::BrokenLink { seq }),
            };
            if linked != prev_hash {
                return Err(ChainError::BrokenLink { seq });
            }
            let stored = body
                .get("hash")
                .and_then(Value::as_str)
                .ok_or(ChainError::Tampered { seq })?;
            if compute_stored_hash(body) != stored {
                return Err(ChainError::Tampered { seq });
            }
            prev_hash = Some(stored);
        }
    }
    Ok(())
}

/// Stored bodies split into one chain per device, keyed by `device_id`. A
/// body that predates the field shares the `None` chain, exactly how a
/// single-device log was read before chains were per device.
fn stored_device_chains(bodies: &[Value]) -> Vec<Vec<&Value>> {
    let mut chains: BTreeMap<Option<&str>, Vec<&Value>> = BTreeMap::new();
    for body in bodies {
        let device = body.get("device_id").and_then(Value::as_str);
        chains.entry(device).or_default().push(body);
    }
    chains.into_values().collect()
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChainError {
    #[error("event seq {seq}: expected seq {expected}")]
    Gap { seq: u64, expected: u64 },
    #[error("event seq {seq}: prev_hash does not match previous event")]
    BrokenLink { seq: u64 },
    #[error("event seq {seq}: stored hash does not match contents")]
    Tampered { seq: u64 },
    #[error("event seq {seq}: belongs to run {found}, expected {expected}")]
    WrongRun {
        seq: u64,
        found: String,
        expected: String,
    },
}

/// Check that `events` (one run) hold intact chains. Events are grouped by
/// `device_id` and each device's chain is checked on its own: seq starts at 1
/// and is gapless, every `prev_hash` links the previous event of the same
/// chain, and every stored hash matches. Chains may arrive in any order, so a
/// multi-device run is several chains rather than one global sequence.
pub fn verify_chain(events: &[Event]) -> Result<(), ChainError> {
    let expected_run = events.first().map(|event| event.run_id.as_str());
    for chain in crate::event::device_chains(events) {
        let mut prev: Option<&Event> = None;
        for (i, e) in chain.into_iter().enumerate() {
            let expected = i as u64 + 1;
            if e.seq != expected {
                return Err(ChainError::Gap { seq: e.seq, expected });
            }
            if let Some(expected_run) = expected_run
                && e.run_id.as_str() != expected_run
            {
                return Err(ChainError::WrongRun {
                    seq: e.seq,
                    found: e.run_id.clone(),
                    expected: expected_run.to_string(),
                });
            }
            if e.prev_hash.as_deref() != prev.map(|p| p.hash.as_str()) {
                return Err(ChainError::BrokenLink { seq: e.seq });
            }
            if compute_hash(e) != e.hash {
                return Err(ChainError::Tampered { seq: e.seq });
            }
            prev = Some(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EventPayload, SCHEMA_VERSION};
    use chrono::Utc;

    fn chain(n: u64) -> Vec<Event> {
        let mut out: Vec<Event> = Vec::new();
        for seq in 1..=n {
            let mut e = Event {
                event_id: format!("e{seq}"),
                schema_version: SCHEMA_VERSION,
                run_id: "r1".into(),
                seq,
                device_id: "d1".into(),
                occurred_at: Utc::now(),
                recorded_at: Utc::now(),
                payload: EventPayload::AgentTurnStarted {
                    session_id: "s".into(),
                },
                prev_hash: out.last().map(|p| p.hash.clone()),
                hash: String::new(),
                sig: None,
            };
            e.hash = compute_hash(&e);
            out.push(e);
        }
        out
    }

    #[test]
    fn canonical_json_sorts_nested_keys() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":{"d":[{"z":1,"y":2}],"c":"x"}}"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":{"c":"x","d":[{"y":2,"z":1}]},"b":1}"#);
    }

    #[test]
    fn intact_chain_verifies() {
        assert_eq!(verify_chain(&chain(5)), Ok(()));
    }

    #[test]
    fn hash_survives_json_roundtrip() {
        let events = chain(3);
        let back: Vec<Event> = serde_json::from_str(&serde_json::to_string(&events).unwrap()).unwrap();
        assert_eq!(verify_chain(&back), Ok(()));
    }

    #[test]
    fn tampering_is_detected() {
        let mut events = chain(3);
        events[1].payload = EventPayload::AgentTurnEnded {
            session_id: "s".into(),
        };
        assert_eq!(verify_chain(&events), Err(ChainError::Tampered { seq: 2 }));
    }

    #[test]
    fn deletion_is_detected() {
        let mut events = chain(3);
        events.remove(1);
        assert_eq!(
            verify_chain(&events),
            Err(ChainError::Gap { seq: 3, expected: 2 })
        );
    }
}
