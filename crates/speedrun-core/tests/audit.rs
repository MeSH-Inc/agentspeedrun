//! Regression tests for v0.1.0 audit findings (docs/audit/v0.1.0.json, docs/V0.2.md).
//!
//! Tests marked `#[ignore = "audit ..."]` fail until their finding is fixed; run them with
//! `cargo test -- --ignored audit_`. The `guard_` tests pass today and must keep passing:
//! they stop a fix from passing an audit test by removing the behaviour it tests.

use chrono::{DateTime, Duration, Utc};
use speedrun_core::hash::compute_hash;
use speedrun_core::redact::redact;
use speedrun_core::{Event, EventPayload, MilestoneOrigin, RunStatus, RunView, SCHEMA_VERSION, Trust};

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
            content: "continue".into(),
            redacted: false,
        },
    )
}

// --- A6-1 (high), V0.2 M5: pausing does not stop the agent ------------------------------

#[test]
#[ignore = "audit A6-1"]
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
#[ignore = "audit A6-1"]
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
#[ignore = "audit A6-1"]
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
#[ignore = "audit A2-3"]
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
#[ignore = "audit A2-4"]
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
            },
        ),
        (
            10,
            EventPayload::MilestoneReached {
                milestone_id: "m".into(),
                trust: Trust::SelfDeclared,
                evidence: Vec::new(),
            },
        ),
    ]
}

#[test]
#[ignore = "audit A6-5"]
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
#[ignore = "audit A1-2"]
fn audit_a1_2_json_quoted_key() {
    assert_redacted(r#"config: {"api_key": "q7Rw2mZx9KpL4vNs"}"#, "q7Rw2mZx9KpL4vNs");
}

#[test]
#[ignore = "audit A1-2"]
fn audit_a1_2_bearer_header() {
    assert_redacted(
        "curl -H 'Authorization: Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0' https://api.example.com",
        "9f8e7d6c5b4a39281706f5e4d3c2b1a0",
    );
}

#[test]
#[ignore = "audit A1-2"]
fn audit_a1_2_credentialed_database_url() {
    assert_redacted(
        "DATABASE_URL is postgres://admin:Vq8sLm2Tn4@db.example.com:5432/app",
        "Vq8sLm2Tn4",
    );
}

#[test]
#[ignore = "audit A1-2"]
fn audit_a1_2_pass_suffix() {
    assert_redacted("export DB_PASS=correcthorsebattery", "correcthorsebattery");
}

#[test]
#[ignore = "audit A1-2"]
fn audit_a1_2_camel_case_key() {
    assert_redacted("const apiKey = \"Zt5pQw8nLr3xVb6y\";", "Zt5pQw8nLr3xVb6y");
}

#[test]
#[ignore = "audit A1-2"]
fn audit_a1_2_stripe_test_and_webhook_keys() {
    assert_redacted("use sk_test_51HxAbCdEfGhIjKlMn for now", "51HxAbCdEfGhIjKlMn");
    assert_redacted(
        "webhook secret whsec_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6",
        "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6",
    );
}

#[test]
#[ignore = "audit A1-2"]
fn audit_a1_2_jwt() {
    assert_redacted(
        "session cookie eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
        "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
    );
}

#[test]
#[ignore = "audit A1-2"]
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
