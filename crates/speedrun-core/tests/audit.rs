//! Regression tests for v0.1.0 audit findings (docs/audit/v0.1.0.json, docs/V0.2.md).
//!
//! Each `audit_` test was ignored until its finding was fixed, then made an ordinary test.
//! The `guard_` tests keep a fix from passing by removing the behaviour under test.

use chrono::{DateTime, Duration, Utc};
use speedrun_core::hash::compute_hash;
use speedrun_core::redact::redact;
use speedrun_core::{
    Claimant, Event, EventPayload, MilestoneOrigin, MilestoneState, RunStatus, RunView, SCHEMA_VERSION, Trust,
};

const MINUTE_MS: i64 = 60_000;

fn t(min: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::minutes(min)
}

/// A run started at minute 0 followed by `items`, in the given (seq) order.
fn log(items: Vec<(i64, EventPayload)>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut all = vec![(
        0,
        EventPayload::RunStarted {
            project_id: "p".into(),
            project_root: None,
            category_id: "any-percent".into(),
            category_version: 1,
            device_pubkey: None,
            start_head: None,
            start_head_committed_at: None,
            ruleset_digest: None,
            disclosure_commitment: None,
        },
    )];
    all.extend(items);
    for (i, (min, payload)) in all.into_iter().enumerate() {
        let mut e = Event {
            event_id: format!("e{i}"),
            schema_version: SCHEMA_VERSION,
            run_id: "r".into(),
            seq: i as u64 + 1,
            device_id: "d".into(),
            occurred_at: t(min),
            recorded_at: t(min),
            payload,
            prev_hash: out.last().map(|p| p.hash.clone()),
            hash: String::new(),
            sig: None,
        };
        e.hash = compute_hash(&e);
        out.push(e);
    }
    out
}

fn fold(items: Vec<(i64, EventPayload)>, now: i64) -> RunView {
    RunView::fold(&log(items), t(now)).expect("log starts with RunStarted")
}

fn started(min: i64) -> (i64, EventPayload) {
    (
        min,
        EventPayload::AgentTurnStarted {
            session_id: "s".into(),
        },
    )
}

fn ended(min: i64) -> (i64, EventPayload) {
    (
        min,
        EventPayload::AgentTurnEnded {
            session_id: "s".into(),
        },
    )
}

fn paused(min: i64) -> (i64, EventPayload) {
    (min, EventPayload::RunPaused { reason: None })
}

fn prompted(min: i64) -> (i64, EventPayload) {
    (
        min,
        EventPayload::PromptSubmitted {
            session_id: "s".into(),
            host: "test".into(),
            model: None,
            content_digest: String::new(),
            length: 8,
            redacted: false,
        },
    )
}

// --- A6-1 (high), V0.2 M5: pausing does not stop the agent ------------------------------

#[test]
fn audit_a6_1_agent_turn_while_paused_resumes_the_run() {
    let v = fold(vec![paused(10), started(20), ended(50)], 60);
    assert_eq!(v.status, RunStatus::Running);
    assert_eq!(
        v.timing.agent_ms,
        30 * MINUTE_MS,
        "agent work while paused counts"
    );
    assert_eq!(
        v.timing.timed_ms,
        50 * MINUTE_MS,
        "only minutes 10-20 were paused"
    );
}

#[test]
fn audit_a6_1_prompt_while_paused_resumes_the_run() {
    let v = fold(vec![paused(10), prompted(20)], 30);
    assert_eq!(v.status, RunStatus::Running);
    assert_eq!(
        v.timing.timed_ms,
        20 * MINUTE_MS,
        "only minutes 10-20 were paused"
    );
}

#[test]
fn audit_a6_1_pause_with_an_open_turn_is_rejected() {
    let v = fold(vec![started(5)], 10);
    assert!(v.check(&EventPayload::RunPaused { reason: None }).is_err());
}

#[test]
fn guard_pause_without_open_turns_still_pauses() {
    let v = fold(vec![started(0), ended(5), paused(10)], 30);
    assert_eq!(v.status, RunStatus::Paused);
    assert_eq!(v.timing.timed_ms, 10 * MINUTE_MS);
    assert_eq!(v.timing.agent_ms, 5 * MINUTE_MS);
    assert!(v.check(&EventPayload::RunResumed {}).is_ok());
}

// --- A2-3 / A2-4, V0.2 W3: turn accounting ----------------------------------------------

#[test]
fn audit_a2_3_a_turn_without_an_end_does_not_absorb_the_gap() {
    // The first turn never ended (interrupt, crash). Its idle hour is not agent time.
    let v = fold(vec![started(0), started(60), ended(70)], 70);
    assert!(
        v.timing.agent_ms < 60 * MINUTE_MS,
        "agent time {} min",
        v.timing.agent_ms / MINUTE_MS
    );
}

#[test]
fn audit_a2_4_turns_pair_by_occurred_at() {
    // Spool replay delivers the first end after the second start: turns 0-30 and 60-80.
    let v = fold(vec![started(0), started(60), ended(30), ended(80)], 90);
    assert_eq!(v.timing.agent_ms, 50 * MINUTE_MS);
}

#[test]
fn guard_ordered_turns_count_once() {
    let v = fold(vec![started(0), ended(30), started(60), ended(80)], 90);
    assert_eq!(v.timing.agent_ms, 50 * MINUTE_MS);
    assert_eq!(v.timing.timed_ms, 90 * MINUTE_MS);
}

// --- A6-5, V0.2 M9: the user's split counts for an agent-proposed milestone ------------

fn milestone(origin: MilestoneOrigin) -> Vec<(i64, EventPayload)> {
    vec![
        (
            1,
            EventPayload::MilestoneProposed {
                milestone_id: "m".into(),
                origin,
                key: None,
                title: "login works".into(),
                description: None,
                required_trust: Trust::AgentClaimed,
            },
        ),
        (
            1,
            EventPayload::MilestoneAccepted {
                milestone_id: "m".into(),
                required_trust: None,
            },
        ),
        (
            10,
            EventPayload::MilestoneReached {
                milestone_id: "m".into(),
                trust: Trust::SelfDeclared,
                evidence: Vec::new(),
                claimant: Claimant::User,
            },
        ),
    ]
}

#[test]
fn audit_a6_5_user_split_counts_for_an_agent_milestone() {
    let v = fold(milestone(MilestoneOrigin::AgentGenerated), 20);
    let m = v.milestone("m").expect("milestone");
    assert_eq!(m.counted_at, Some(t(10)));
}

#[test]
fn guard_self_declared_claim_does_not_count_for_a_predefined_milestone() {
    let v = fold(milestone(MilestoneOrigin::Predefined), 20);
    assert_eq!(v.milestone("m").expect("milestone").counted_at, None);
}

// --- A1-2, V0.2 M7: redaction misses common secret shapes -------------------------------

fn assert_redacted(text: &str, secret: &str) {
    let (out, changed) = redact(text);
    assert!(changed, "nothing redacted in {text:?}");
    assert!(!out.contains(secret), "{secret:?} survives in {out:?}");
}

#[test]
fn audit_a1_2_json_quoted_key() {
    assert_redacted(r#"config: {"api_key": "q7Rw2mZx9KpL4vNs"}"#, "q7Rw2mZx9KpL4vNs");
}

#[test]
fn audit_a1_2_bearer_header() {
    assert_redacted(
        "curl -H 'Authorization: Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0' https://api.example.com",
        "9f8e7d6c5b4a39281706f5e4d3c2b1a0",
    );
}

#[test]
fn audit_a1_2_credentialed_database_url() {
    assert_redacted(
        "DATABASE_URL is postgres://admin:Vq8sLm2Tn4@db.example.com:5432/app",
        "Vq8sLm2Tn4",
    );
}

#[test]
fn audit_a1_2_pass_suffix() {
    assert_redacted("export DB_PASS=correcthorsebattery", "correcthorsebattery");
}

#[test]
fn audit_a1_2_camel_case_key() {
    assert_redacted("const apiKey = \"Zt5pQw8nLr3xVb6y\";", "Zt5pQw8nLr3xVb6y");
}

#[test]
fn audit_a1_2_stripe_test_and_webhook_keys() {
    assert_redacted("use sk_test_51HxAbCdEfGhIjKlMn for now", "51HxAbCdEfGhIjKlMn");
    assert_redacted(
        "webhook secret whsec_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6",
        "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6",
    );
}

#[test]
fn audit_a1_2_jwt() {
    assert_redacted(
        "session cookie eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    );
}

#[test]
fn audit_a1_2_truncated_private_key() {
    assert_redacted(
        "here is the key: -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB",
        "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAAB",
    );
}

#[test]
fn guard_redaction_leaves_ordinary_prompts_alone() {
    for text in [
        "I hate to be the bearer of bad news, but the build is red",
        "implement a token bucket rate limiter with a refill rate setting",
        "connect to postgres://localhost:5432/app and list the tables",
        r#"the config is {"name": "todo app", "port": 3000}"#,
        "rename apiClient to httpClient and set PAGE_SIZE=50",
        "add a password field and a passphrase hint to the signup form",
    ] {
        assert_eq!(redact(text), (text.to_string(), false), "changed {text:?}");
    }
}

// --- A3-5 / A6-5, V0.2 M9: accepting a milestone can set the trust it requires ----------

/// An agent-proposed milestone requiring `agent_claimed`, then `accepted`: a
/// `milestone_accepted` payload as JSON, so it can carry fields the payload may gain.
fn accepted(accepted: serde_json::Value) -> RunView {
    let accepted: EventPayload = serde_json::from_value(accepted).expect("a milestone_accepted payload");
    let mut items = milestone(MilestoneOrigin::AgentGenerated);
    items.truncate(1);
    items.push((2, accepted));
    fold(items, 20)
}

#[test]
fn audit_m9_b_accept_sets_required_trust() {
    // END_STATE M9.b; V0.2 M9: "Let `accept` set required_trust", raising or lowering it.
    for (wire, trust) in [
        ("evidence_verified", Trust::EvidenceVerified),
        ("self_declared", Trust::SelfDeclared),
    ] {
        let v = accepted(serde_json::json!({
            "type": "milestone_accepted",
            "milestone_id": "m",
            "required_trust": wire,
        }));
        let m = v.milestone("m").expect("milestone");
        assert_eq!(m.state, MilestoneState::Accepted);
        assert_eq!(m.required_trust, trust, "accepted with required_trust {wire}");
    }
}

#[test]
fn guard_accept_without_required_trust_keeps_the_proposed_requirement() {
    // v0.1 logs carry no required_trust on milestone_accepted; they must fold as before.
    let v = accepted(serde_json::json!({ "type": "milestone_accepted", "milestone_id": "m" }));
    let m = v.milestone("m").expect("milestone");
    assert_eq!(m.state, MilestoneState::Accepted);
    assert_eq!(m.required_trust, Trust::AgentClaimed);
}

// --- W1.a, A2-5: hash and verify the stored body, not its typed re-serialization -------

/// sha256 over the canonical JSON of `body` without `hash` and `sig`, computed here rather
/// than by the code under test.
fn expected_stored_hash(body: &serde_json::Value) -> String {
    use sha2::Digest;
    let mut v = body.clone();
    let map = v.as_object_mut().expect("an event body is an object");
    map.remove("hash");
    map.remove("sig");
    hex::encode(sha2::Sha256::digest(
        speedrun_core::hash::canonical_json(&v).as_bytes(),
    ))
}

/// Three stored bodies of one run, the second carrying `extra` (fields this build does not
/// know) in its payload, each hashed as stored and linked to the one before.
fn stored_chain(extra: serde_json::Value) -> Vec<serde_json::Value> {
    let mut bodies: Vec<serde_json::Value> = Vec::new();
    for (seq, e) in log(vec![ended(1), ended(2)]).into_iter().enumerate() {
        let mut body = serde_json::to_value(&e).expect("an event serializes");
        if seq == 1 {
            let payload = body["payload"].as_object_mut().expect("payload object");
            payload.extend(extra.as_object().expect("extra fields").clone());
        }
        body["prev_hash"] = bodies
            .last()
            .map_or(serde_json::Value::Null, |p| p["hash"].clone());
        body["hash"] = serde_json::Value::String(expected_stored_hash(&body));
        bodies.push(body);
    }
    bodies
}

#[test]
fn audit_w1_a_stored_hash_keeps_fields_this_build_does_not_know() {
    let body = &stored_chain(serde_json::json!({ "future_field": "x" }))[1];
    assert_eq!(
        speedrun_core::hash::compute_stored_hash(body),
        expected_stored_hash(body)
    );
}

#[test]
fn audit_w1_a_stored_chain_with_an_unknown_optional_field_verifies() {
    let bodies = stored_chain(serde_json::json!({ "future_field": "x", "future_list": [1, 2] }));
    assert_eq!(speedrun_core::hash::verify_stored_chain(&bodies), Ok(()));
}

#[test]
fn audit_w1_a_editing_an_unknown_field_is_detected() {
    let mut bodies = stored_chain(serde_json::json!({ "future_field": "x" }));
    bodies[1]["payload"]["future_field"] = serde_json::json!("y");
    assert_eq!(
        speedrun_core::hash::verify_stored_chain(&bodies),
        Err(speedrun_core::hash::ChainError::Tampered { seq: 2 })
    );
}

#[test]
fn audit_w1_a_stored_chain_detects_gaps_and_broken_links() {
    let mut gap = stored_chain(serde_json::json!({}));
    gap.remove(1);
    assert_eq!(
        speedrun_core::hash::verify_stored_chain(&gap),
        Err(speedrun_core::hash::ChainError::Gap { seq: 3, expected: 2 })
    );
    let mut relinked = stored_chain(serde_json::json!({}));
    relinked[2]["prev_hash"] = serde_json::json!("0".repeat(64));
    relinked[2]["hash"] = serde_json::Value::String(expected_stored_hash(&relinked[2]));
    assert_eq!(
        speedrun_core::hash::verify_stored_chain(&relinked),
        Err(speedrun_core::hash::ChainError::BrokenLink { seq: 3 })
    );
}

#[test]
fn guard_w1_a_typed_chain_still_verifies() {
    let events = log(vec![ended(1), ended(2)]);
    assert_eq!(speedrun_core::hash::verify_chain(&events), Ok(()));
}

// --- W1.e, owner decision D4: v1 formats are retired, so an unsigned event fails ---------

/// A run whose events are all signed by one key, as the v2 daemon writes them.
fn signed_log(key: &ed25519_dalek::SigningKey) -> Vec<Event> {
    use speedrun_core::sign::{pubkey_hex, sign_hash};
    let mut out: Vec<Event> = Vec::new();
    for (seq, mut e) in log(vec![ended(1), ended(2)]).into_iter().enumerate() {
        if seq == 0 {
            e.payload = EventPayload::RunStarted {
                project_id: "p".into(),
                project_root: None,
                category_id: "any-percent".into(),
                category_version: 1,
                device_pubkey: Some(pubkey_hex(key)),
                start_head: None,
                start_head_committed_at: None,
                ruleset_digest: None,
                disclosure_commitment: None,
            };
        }
        e.prev_hash = out.last().map(|p| p.hash.clone());
        e.hash = compute_hash(&e);
        e.sig = Some(sign_hash(key, &e.hash));
        out.push(e);
    }
    out
}

fn device_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[9u8; 32])
}

#[test]
fn audit_w1_e_run_without_a_device_key_fails_the_signature_check() {
    // log() writes a RunStarted with no device_pubkey and no signatures: a v1 log.
    let check = speedrun_core::sign::verify_event_signatures(&log(vec![ended(1)]));
    assert!(check.error.is_some(), "an unsigned v1 log must fail: {check:?}");
}

#[test]
fn audit_w1_e_one_unsigned_event_in_a_signed_run_fails_the_signature_check() {
    let mut events = signed_log(&device_key());
    events[2].sig = None;
    let check = speedrun_core::sign::verify_event_signatures(&events);
    assert!(
        check.error.as_deref().is_some_and(|e| e.contains("seq 3")),
        "the unsigned event must be named: {check:?}"
    );
}

#[test]
fn guard_w1_e_fully_signed_run_passes_the_signature_check() {
    let check = speedrun_core::sign::verify_event_signatures(&signed_log(&device_key()));
    assert!(check.ok(), "{check:?}");
    assert_eq!(check.signed, 3);
}

// --- W1.d, D7: HMAC-SHA256 for prompt digests and disclosure commitments -----------------

#[test]
fn audit_w1_d_hmac_sha256_matches_rfc_4231() {
    use speedrun_core::digest::hmac_sha256;
    // RFC 4231 test cases 1, 2 and 6 (a key longer than the block size).
    let cases: [(Vec<u8>, &[u8], &str); 3] = [
        (
            vec![0x0b; 20],
            b"Hi There",
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        ),
        (
            b"Jefe".to_vec(),
            b"what do ya want for nothing?",
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
        ),
        (
            vec![0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
        ),
    ];
    for (key, message, expected) in cases {
        assert_eq!(hex::encode(hmac_sha256(&key, message)), expected);
    }
}

// --- W9.c, P0.a: key rotation and several device chains in one run ----------------------

fn key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// The certificate format, computed here: the old key signs the prefix and the new key.
fn certificate(old: &ed25519_dalek::SigningKey, new: &ed25519_dalek::SigningKey) -> String {
    use ed25519_dalek::Signer;
    let message = format!(
        "{}{}",
        speedrun_core::sign::ROTATION_MESSAGE_PREFIX,
        speedrun_core::sign::pubkey_hex(new)
    );
    hex::encode(old.sign(message.as_bytes()).to_bytes())
}

/// One device's chain: `(minute, payload, signing key)` in seq order, linked and signed.
fn chain(device: &str, items: Vec<(i64, EventPayload, &ed25519_dalek::SigningKey)>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    for (i, (min, payload, signer)) in items.into_iter().enumerate() {
        let mut e = Event {
            event_id: format!("{device}-{i}"),
            schema_version: SCHEMA_VERSION,
            run_id: "r".into(),
            seq: i as u64 + 1,
            device_id: device.into(),
            occurred_at: t(min),
            recorded_at: t(min),
            payload,
            prev_hash: out.last().map(|p| p.hash.clone()),
            hash: String::new(),
            sig: None,
        };
        e.hash = compute_hash(&e);
        e.sig = Some(speedrun_core::sign::sign_hash(signer, &e.hash));
        out.push(e);
    }
    out
}

fn run_started(signer: &ed25519_dalek::SigningKey) -> EventPayload {
    EventPayload::RunStarted {
        project_id: "p".into(),
        project_root: None,
        category_id: "any-percent".into(),
        category_version: 1,
        device_pubkey: Some(speedrun_core::sign::pubkey_hex(signer)),
        start_head: None,
        start_head_committed_at: None,
        ruleset_digest: None,
        disclosure_commitment: None,
    }
}

fn rotated(old: &ed25519_dalek::SigningKey, new: &ed25519_dalek::SigningKey) -> EventPayload {
    EventPayload::DeviceRotated {
        new_pubkey: speedrun_core::sign::pubkey_hex(new),
        certificate: certificate(old, new),
    }
}

fn turn(start: bool) -> EventPayload {
    if start {
        EventPayload::AgentTurnStarted {
            session_id: "s".into(),
        }
    } else {
        EventPayload::AgentTurnEnded {
            session_id: "s".into(),
        }
    }
}

#[test]
fn audit_w9_c_rotation_certificate_is_the_old_keys_signature_over_the_new_key() {
    use speedrun_core::sign::{pubkey_hex, rotation_certificate, verify_rotation};
    let (old, new, other) = (key(1), key(2), key(3));
    assert_eq!(
        rotation_certificate(&old, &pubkey_hex(&new)),
        certificate(&old, &new)
    );
    let cert = certificate(&old, &new);
    assert!(verify_rotation(&pubkey_hex(&old), &pubkey_hex(&new), &cert));
    assert!(!verify_rotation(&pubkey_hex(&other), &pubkey_hex(&new), &cert));
    assert!(!verify_rotation(&pubkey_hex(&old), &pubkey_hex(&other), &cert));
    assert!(!verify_rotation(&pubkey_hex(&old), &pubkey_hex(&new), "not hex"));
}

#[test]
fn audit_w9_c_rotated_chain_verifies_from_public_keys_alone() {
    let (old, new) = (key(1), key(2));
    let events = chain(
        "d1",
        vec![
            (0, run_started(&old), &old),
            (1, turn(true), &old),
            (2, rotated(&old, &new), &new),
            (3, turn(false), &new),
        ],
    );
    assert_eq!(speedrun_core::hash::verify_chain(&events), Ok(()));
    let check = speedrun_core::sign::verify_event_signatures(&events);
    assert!(check.ok(), "{check:?}");
    assert_eq!(check.signed, 4);
}

#[test]
fn guard_w9_c_rotation_with_a_forged_certificate_fails() {
    let (old, new, thief) = (key(1), key(2), key(3));
    let mut forged = rotated(&old, &new);
    if let EventPayload::DeviceRotated { certificate: c, .. } = &mut forged {
        *c = certificate(&thief, &new);
    }
    let events = chain(
        "d1",
        vec![
            (0, run_started(&old), &old),
            (1, forged, &new),
            (2, turn(true), &new),
        ],
    );
    assert!(
        speedrun_core::sign::verify_event_signatures(&events)
            .error
            .is_some()
    );
}

#[test]
fn guard_w9_c_old_key_after_rotation_fails() {
    let (old, new) = (key(1), key(2));
    let events = chain(
        "d1",
        vec![
            (0, run_started(&old), &old),
            (1, rotated(&old, &new), &new),
            (2, turn(true), &old),
        ],
    );
    assert!(
        speedrun_core::sign::verify_event_signatures(&events)
            .error
            .is_some()
    );
}

/// Device d1 starts the run and pauses it at minute 5; device d2 joins and runs an agent
/// turn from minute 3 to 4. Chains are given d2's first, each in seq order.
fn two_device_run() -> Vec<Event> {
    let (a, b) = (key(1), key(2));
    let joined = EventPayload::DeviceJoined {
        device_pubkey: speedrun_core::sign::pubkey_hex(&b),
    };
    let mut events = chain(
        "d2",
        vec![(2, joined, &b), (3, turn(true), &b), (4, turn(false), &b)],
    );
    events.extend(chain(
        "d1",
        vec![
            (0, run_started(&a), &a),
            (5, EventPayload::RunPaused { reason: None }, &a),
        ],
    ));
    events
}

#[test]
fn audit_w9_c_two_device_chains_in_one_run_verify() {
    let events = two_device_run();
    assert_eq!(speedrun_core::hash::verify_chain(&events), Ok(()));
    let check = speedrun_core::sign::verify_event_signatures(&events);
    assert!(check.ok(), "{check:?}");
    assert_eq!(check.signed, 5);
}

#[test]
fn guard_w9_c_tampering_in_a_second_chain_is_detected() {
    let mut events = two_device_run();
    events[1].payload = turn(false);
    assert!(speedrun_core::hash::verify_chain(&events).is_err());
    let mut events = two_device_run();
    events.remove(1);
    assert!(speedrun_core::hash::verify_chain(&events).is_err());
}

#[test]
fn audit_w9_c_fold_merges_device_chains_by_occurred_at() {
    // In time order the turn (minutes 3-4) ends before the pause (minute 5), so the run
    // ends paused with one minute of agent time. Read chain by chain, the turn would come
    // after the pause and resume the run.
    let v = RunView::fold(&two_device_run(), t(10)).expect("a run");
    assert_eq!(v.status, RunStatus::Paused);
    assert_eq!(v.timing.agent_ms, MINUTE_MS);
}

// --- P1.c: the commons log is an RFC 9162 Merkle tree (COMMONS.md §19) ------------------
//
// The reference below follows RFC 9162 §2.1 directly and shares no code with
// `speedrun_core::merkle`, so the module is checked against the RFC, not itself.

mod rfc_9162 {
    use sha2::{Digest, Sha256};

    pub fn hash(bytes: &[u8]) -> Vec<u8> {
        Sha256::digest(bytes).to_vec()
    }

    pub fn leaf(data: &[u8]) -> Vec<u8> {
        hash(&[&[0u8][..], data].concat())
    }

    fn node(left: &[u8], right: &[u8]) -> Vec<u8> {
        hash(&[&[1u8][..], left, right].concat())
    }

    /// The largest power of two smaller than `n` (n > 1).
    fn split(n: usize) -> usize {
        let mut k = 1;
        while k * 2 < n {
            k *= 2;
        }
        k
    }

    pub fn root(leaves: &[Vec<u8>]) -> Vec<u8> {
        match leaves.len() {
            0 => hash(b""),
            1 => leaves[0].clone(),
            n => {
                let k = split(n);
                node(&root(&leaves[..k]), &root(&leaves[k..]))
            }
        }
    }

    pub fn path(m: usize, leaves: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let n = leaves.len();
        if n <= 1 {
            return Vec::new();
        }
        let k = split(n);
        if m < k {
            let mut p = path(m, &leaves[..k]);
            p.push(root(&leaves[k..]));
            p
        } else {
            let mut p = path(m - k, &leaves[k..]);
            p.push(root(&leaves[..k]));
            p
        }
    }
}

fn hex_all(hashes: &[Vec<u8>]) -> Vec<String> {
    hashes.iter().map(hex::encode).collect()
}

#[test]
fn audit_p1_c_a_leaf_is_the_rfc_9162_leaf_of_the_publication_text() {
    let (bundle, judgment) = ("ab".repeat(32), "cd".repeat(32));
    let expected = hex::encode(rfc_9162::leaf(format!("{bundle}:{judgment}").as_bytes()));
    assert_eq!(speedrun_core::merkle::leaf_hash(&bundle, &judgment), expected);
}

#[test]
fn audit_p1_c_roots_and_inclusion_proofs_match_rfc_9162() {
    use speedrun_core::merkle;
    for n in 0..=17usize {
        let raw: Vec<Vec<u8>> = (0..n)
            .map(|i| rfc_9162::leaf(format!("leaf {i}").as_bytes()))
            .collect();
        let leaves = hex_all(&raw);
        let root = hex::encode(rfc_9162::root(&raw));
        assert_eq!(merkle::root(&leaves), Some(root.clone()), "root of {n} leaves");
        assert_eq!(
            merkle::inclusion_proof(n as u64, &leaves),
            None,
            "leaf {n} of {n}"
        );
        for m in 0..n {
            let expected = hex_all(&rfc_9162::path(m, &raw));
            let proof = merkle::inclusion_proof(m as u64, &leaves);
            assert_eq!(proof.as_ref(), Some(&expected), "path of leaf {m} of {n}");
            let size = n as u64;
            assert!(merkle::verify_inclusion(
                &leaves[m], m as u64, size, &expected, &root
            ));
            // A proof for a tree of `n` can also verify at size `n + 1` (RFC
            // 9162 cannot tell a node hash from a leaf hash), so the negatives
            // here are the ones every correct verifier refuses.
            assert!(!merkle::verify_inclusion(
                &leaves[m],
                m as u64 + size,
                size,
                &expected,
                &root
            ));
            let mut longer = expected.clone();
            longer.push(hex::encode(rfc_9162::hash(b"one hash too many")));
            assert!(!merkle::verify_inclusion(
                &leaves[m], m as u64, size, &longer, &root
            ));
            if let Some((_, shorter)) = expected.split_last() {
                assert!(!merkle::verify_inclusion(
                    &leaves[m], m as u64, size, shorter, &root
                ));
            }
            if n > 1 {
                let other = (m + 1) % n;
                assert!(!merkle::verify_inclusion(
                    &leaves[m],
                    other as u64,
                    size,
                    &expected,
                    &root
                ));
                let mut tampered = expected.clone();
                tampered[0] = hex::encode(rfc_9162::hash(b"not a sibling"));
                assert!(!merkle::verify_inclusion(
                    &leaves[m], m as u64, size, &tampered, &root
                ));
            }
        }
    }
}

#[test]
fn guard_p1_c_garbage_never_verifies() {
    assert!(!speedrun_core::merkle::verify_inclusion("zz", 0, 1, &[], "zz"));
}
