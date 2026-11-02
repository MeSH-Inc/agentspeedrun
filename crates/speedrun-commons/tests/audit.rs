//! Objective tests for the commons server (COMMONS.md §19; END_STATE.md P1), run against
//! the `speedrun-commons` binary over loopback HTTP and against its library.
//!
//! Tests marked `#[ignore = "audit ..."]` fail until their item holds; run them with
//! `cargo test -p speedrun-commons -- --ignored audit_`. The `guard_` tests pass today.

use std::fs::File;
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use speedrun_core::api::PromptCommitment;
use speedrun_core::bundle::{self, BUNDLE_FORMAT, Bundle, ChainHead};
use speedrun_core::commons::{
    CommonsHealth, InclusionProof, PublishReceipt, PublishedRun, TreeHead, judgment_digest, tree_head_digest,
};
use speedrun_core::{
    Claimant, Event, EventPayload, Evidence, MilestoneOrigin, RunOutcome, RunView, SCHEMA_VERSION, Trust,
    hash, sign,
};

// --- A run's log, written as speedrund writes one ------------------------------------------

fn t(minute: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).unwrap() + chrono::Duration::minutes(minute)
}

fn milestone(key: &str) -> String {
    format!("any-percent@1/{key}")
}

/// One device's signed chain for one any% run.
struct Log {
    key: SigningKey,
    run_id: String,
    events: Vec<Event>,
}

impl Log {
    /// A run started at minute 0 with any%'s two milestones proposed and accepted.
    fn start(run_id: &str) -> Self {
        Self::start_with_root(run_id, None)
    }

    fn start_with_root(run_id: &str, project_root: Option<&str>) -> Self {
        let key = sign::generate();
        let mut log = Self {
            run_id: run_id.into(),
            events: Vec::new(),
            key,
        };
        log.push(
            0,
            EventPayload::RunStarted {
                project_id: "01KCOMMONSPROJECT00000000".into(),
                project_root: project_root.map(str::to_string),
                category_id: "any-percent".into(),
                category_version: 1,
                device_pubkey: Some(sign::pubkey_hex(&log.key)),
                start_head: None,
                start_head_committed_at: None,
                ruleset_digest: None,
                disclosure_commitment: Some("d1".repeat(32)),
            },
        );
        for (key, required) in [
            ("scaffold-runs", Trust::SelfDeclared),
            ("happy-path-demo", Trust::EvidenceVerified),
        ] {
            log.push(
                0,
                EventPayload::MilestoneProposed {
                    milestone_id: milestone(key),
                    origin: MilestoneOrigin::Predefined,
                    key: Some(key.into()),
                    title: key.into(),
                    description: None,
                    required_trust: required,
                },
            );
            log.push(
                0,
                EventPayload::MilestoneAccepted {
                    milestone_id: milestone(key),
                    required_trust: None,
                },
            );
        }
        log
    }

    fn push(&mut self, minute: i64, payload: EventPayload) {
        let pubkey = sign::pubkey_hex(&self.key);
        let mut event = Event {
            event_id: format!("{}:{}", self.run_id, self.events.len() + 1),
            schema_version: SCHEMA_VERSION,
            run_id: self.run_id.clone(),
            seq: self.events.len() as u64 + 1,
            device_id: sign::fingerprint(&pubkey).unwrap(),
            occurred_at: t(minute),
            recorded_at: t(minute),
            payload,
            prev_hash: self.events.last().map(|e| e.hash.clone()),
            hash: String::new(),
            sig: None,
        };
        event.hash = hash::compute_hash(&event);
        event.sig = Some(sign::sign_hash(&self.key, &event.hash));
        self.events.push(event);
    }

    fn prompt(mut self, minute: i64) -> Self {
        self.push(
            minute,
            EventPayload::PromptSubmitted {
                session_id: "s1".into(),
                host: "test".into(),
                model: None,
                content_digest: "00".repeat(32),
                length: 12,
                redacted: false,
            },
        );
        self
    }

    fn claim(mut self, minute: i64, key: &str, evidence: Vec<Evidence>) -> Self {
        self.push(
            minute,
            EventPayload::MilestoneReached {
                milestone_id: milestone(key),
                trust: Trust::SelfDeclared,
                evidence,
                claimant: Claimant::User,
            },
        );
        self
    }

    /// A verification the author's own device signed without any verifier running.
    fn forged_verification(mut self, minute: i64, key: &str, verifier: &str) -> Self {
        self.push(
            minute,
            EventPayload::MilestoneVerified {
                milestone_id: milestone(key),
                trust: Trust::EvidenceVerified,
                verifier: verifier.into(),
                evidence: vec![],
            },
        );
        self
    }

    fn finish(mut self, minute: i64) -> Self {
        self.push(
            minute,
            EventPayload::RunFinished {
                outcome: RunOutcome::Abandoned,
                reason: None,
            },
        );
        self
    }

    fn bodies(&self) -> Vec<Value> {
        self.events
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect()
    }

    /// The bundle speedrund would build for this run, signed by the run's own key.
    fn bundle(&self, tier: u8) -> Bundle {
        let prompts = if tier == 2 {
            self.events
                .iter()
                .filter(|e| matches!(e.payload, EventPayload::PromptSubmitted { .. }))
                .map(|e| PromptCommitment {
                    event_id: e.event_id.clone(),
                    commitment: "c0".repeat(32),
                    length: 12,
                    redacted: false,
                })
                .collect()
        } else {
            vec![]
        };
        let unsigned = Bundle {
            format: BUNDLE_FORMAT,
            run_id: self.run_id.clone(),
            tier,
            events: self.bodies(),
            chain_heads: vec![],
            prompts,
            publisher_pubkey: String::new(),
            signature: String::new(),
        };
        signed(unsigned, &self.key)
    }
}

impl Log {
    /// The head of this log's chain: its last event's device, sequence number and hash.
    fn head(&self) -> ChainHead {
        let bodies = self.bodies();
        let last = bodies.last().expect("a log has events");
        ChainHead {
            device_id: last["device_id"].as_str().unwrap().to_string(),
            seq: last["seq"].as_u64().unwrap(),
            hash: last["hash"].as_str().unwrap().to_string(),
        }
    }

    /// A tier 0 bundle anchoring `heads`, signed by this log's key.
    fn heads_bundle(&self, heads: Vec<ChainHead>) -> Bundle {
        let unsigned = Bundle {
            format: BUNDLE_FORMAT,
            run_id: self.run_id.clone(),
            tier: 0,
            events: vec![],
            chain_heads: heads,
            prompts: vec![],
            publisher_pubkey: String::new(),
            signature: String::new(),
        };
        signed(unsigned, &self.key)
    }
}

/// `bundle` with `publisher_pubkey` set to `key`'s and signed by it.
fn signed(mut bundle: Bundle, key: &SigningKey) -> Bundle {
    bundle.publisher_pubkey = sign::pubkey_hex(key);
    bundle.signature = sign::sign_hash(key, &bundle::digest(&bundle).unwrap());
    bundle
}

/// A finished run with one prompt and nothing claimed.
fn plain_run(run_id: &str) -> Log {
    Log::start(run_id).prompt(1).finish(30)
}

/// A finished run with one prompt and one claim, made at `minute` before the finish.
fn claimed_run(run_id: &str, minute: i64, key: &str, evidence: Vec<Evidence>) -> Log {
    Log::start(run_id)
        .prompt(1)
        .claim(minute, key, evidence)
        .finish(30)
}

/// The fixture's logs are what the contract asks of a publishable run, so every refusal
/// the tests below expect comes from the one thing each case breaks.
#[test]
fn guard_p1_the_test_logs_verify_with_public_keys_alone() {
    for log in [
        plain_run("01RUNGUARD"),
        claimed_run("01RUNGUARDCLAIM", 5, "scaffold-runs", vec![]),
    ] {
        let bundle = log.bundle(2);
        assert!(sign::verify_hash(
            &bundle.publisher_pubkey,
            &bundle::digest(&bundle).unwrap(),
            &bundle.signature
        ));
        hash::verify_stored_chain(&bundle.events).expect("intact chain");
        assert!(sign::verify_event_signatures(&log.events).ok());
        let view = RunView::fold(&log.events, t(30)).expect("folds");
        assert!(view.is_finished());
        assert_eq!(bundle.prompts.len(), 1);
    }
}

// --- The server --------------------------------------------------------------------------

const STARTUP_LIMIT: Duration = Duration::from_secs(15);

struct Commons {
    child: Child,
    base: String,
    _home: tempfile::TempDir,
}

impl Drop for Commons {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("a free loopback port")
}

/// Send one request; transport failures give status 0. Bodies that are not JSON give `Null`.
fn request(method: &str, url: &str, body: Option<&Value>) -> (u16, Value) {
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
    let req = agent.request(method, url);
    let result = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(_)) => return (0, Value::Null),
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

impl Commons {
    fn start() -> Self {
        for _ in 0..5 {
            if let Some(c) = Self::try_start() {
                return c;
            }
        }
        panic!("speedrun-commons did not start on any of five free ports");
    }

    fn try_start() -> Option<Self> {
        let home = tempfile::tempdir().unwrap();
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_speedrun-commons"))
            .env("SPEEDRUN_COMMONS_HOME", home.path().join("data"))
            .env("SPEEDRUN_COMMONS_ADDR", format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(home.path().join("stderr")).unwrap())
            .spawn()
            .expect("spawn speedrun-commons");
        let mut commons = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            _home: home,
        };
        let deadline = Instant::now() + STARTUP_LIMIT;
        while Instant::now() < deadline {
            if !matches!(commons.child.try_wait(), Ok(None)) {
                return None;
            }
            if commons.get("/v1/health").0 == 200 {
                return Some(commons);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    fn get(&self, path: &str) -> (u16, Value) {
        request("GET", &format!("{}{path}", self.base), None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        request("POST", &format!("{}{path}", self.base), Some(body))
    }

    fn health(&self) -> CommonsHealth {
        let (status, body) = self.get("/v1/health");
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }

    /// Publish `bundle`, asserting it was accepted.
    fn publish(&self, bundle: &Bundle) -> PublishReceipt {
        let (status, body) = self.post("/v1/bundles", &serde_json::to_value(bundle).unwrap());
        assert_eq!(status, 200, "publishing {}: {body}", bundle.run_id);
        serde_json::from_value(body.clone()).unwrap_or_else(|e| panic!("not a receipt: {e}: {body}"))
    }
}

fn judged<'a>(receipt: &'a PublishReceipt, key: &str) -> &'a speedrun_core::commons::MilestoneJudgment {
    receipt
        .judgment
        .milestones
        .iter()
        .find(|m| m.milestone_id == milestone(key))
        .unwrap_or_else(|| panic!("no judgment of {key}: {:?}", receipt.judgment))
}

#[test]
fn guard_p1_the_commons_answers_health_with_its_key() {
    let commons = Commons::start();
    let health = commons.health();
    assert!(health.ok);
    assert_eq!(health.commons_pubkey.len(), 64);
    assert_eq!(health.tree_size, 0);
}

// --- P1.a: the commons judges claims itself ----------------------------------------------

#[test]
fn audit_p1_a_a_forged_evidence_verified_claim_is_not_granted() {
    let commons = Commons::start();
    let by_commit = Log::start("01RUNFORGEDCOMMIT")
        .prompt(1)
        .claim(
            10,
            "happy-path-demo",
            vec![Evidence::GitCommit {
                sha: "4f2a9c1".into(),
            }],
        )
        .forged_verification(11, "happy-path-demo", "git")
        .finish(30);
    let by_url = Log::start("01RUNFORGEDURL")
        .prompt(1)
        .claim(
            10,
            "happy-path-demo",
            vec![Evidence::Url {
                url: "https://127.0.0.1/demo".into(),
            }],
        )
        .forged_verification(11, "happy-path-demo", "url")
        .finish(30);
    for log in [by_commit, by_url] {
        let receipt = commons.publish(&log.bundle(1));
        let demo = judged(&receipt, "happy-path-demo");
        assert_eq!(demo.claimed, Some(Trust::EvidenceVerified), "{demo:?}");
        assert_eq!(
            demo.granted,
            Some(Trust::AgentClaimed),
            "{}: {demo:?}",
            log.run_id
        );
        assert!(!demo.problems.is_empty(), "{demo:?}");
    }
}

#[test]
fn audit_p1_a_a_judgment_is_signed_by_the_commons_key() {
    let commons = Commons::start();
    let health = commons.health();
    let log = claimed_run("01RUNSIGNED", 5, "scaffold-runs", vec![]);
    let bundle = log.bundle(1);
    let receipt = commons.publish(&bundle);
    let judgment = &receipt.judgment;
    assert_eq!(receipt.bundle_digest, bundle::digest(&bundle).unwrap());
    assert_eq!(judgment.bundle_digest, receipt.bundle_digest);
    assert_eq!(judgment.run_id, "01RUNSIGNED");
    assert_eq!(judgment.verifier_pubkey, health.commons_pubkey);
    assert!(sign::verify_hash(
        &health.commons_pubkey,
        &judgment_digest(judgment).unwrap(),
        &judgment.signature
    ));
    assert!(sign::verify_hash(
        &health.commons_pubkey,
        &tree_head_digest(&receipt.tree_head).unwrap(),
        &receipt.tree_head.signature
    ));
    let scaffold = judged(&receipt, "scaffold-runs");
    assert_eq!(
        (scaffold.claimed, scaffold.granted),
        (Some(Trust::SelfDeclared), Some(Trust::SelfDeclared))
    );
    let demo = judged(&receipt, "happy-path-demo");
    assert_eq!((demo.claimed, demo.granted), (None, None));
}

/// Canned pages by URL; anything else is unreachable.
struct Pages(Vec<(&'static str, u16, String)>);

impl speedrun_verify::Fetch for Pages {
    fn get(&self, url: &str, _headers: &[(&str, &str)], _api: bool) -> Result<speedrun_verify::Page, String> {
        self.0
            .iter()
            .find(|(u, _, _)| *u == url)
            .map(|(u, status, body)| speedrun_verify::Page {
                status: *status,
                url: (*u).to_string(),
                body: body.clone(),
            })
            .ok_or_else(|| format!("{url} is unreachable"))
    }
}

struct NoSecrets;

impl speedrun_verify::Secrets for NoSecrets {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}

#[test]
fn guard_p1_concurrent_publications_each_get_their_own_leaf() {
    let commons = Commons::start();
    let bundles: Vec<Bundle> = (0..8)
        .map(|i| plain_run(&format!("01RUNCONCURRENT{i}")).bundle(1))
        .collect();
    let receipts: Vec<PublishReceipt> = std::thread::scope(|scope| {
        let handles: Vec<_> = bundles
            .iter()
            .map(|bundle| scope.spawn(|| commons.publish(bundle)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut leaves: Vec<u64> = receipts.iter().map(|r| r.inclusion.leaf_index).collect();
    leaves.sort();
    assert_eq!(leaves, (0..8).collect::<Vec<u64>>());
    assert_eq!(commons.health().tree_size, 8);
}

#[test]
fn guard_p1_the_commons_judges_with_no_secrets() {
    let verifier = speedrun_commons::judge::verifier();
    for name in ["github", "stripe", "anything"] {
        assert_eq!(
            verifier.secrets.get(name),
            None,
            "the commons verifier holds a {name} secret"
        );
    }
}

fn judge_with(pages: Pages, log: &Log) -> Vec<speedrun_core::commons::MilestoneJudgment> {
    let verifier = speedrun_verify::Verifier {
        fetch: Box::new(pages),
        secrets: Box::new(NoSecrets),
    };
    let view = RunView::fold(&log.events, t(30)).unwrap();
    speedrun_commons::judge::judge(&view, &log.events, &verifier)
}

#[test]
fn audit_p1_a_the_commons_grants_evidence_its_own_verifier_passes() {
    const DEMO: &str = "https://recipes.acme.dev/";
    let log = claimed_run(
        "01RUNDEMO",
        10,
        "happy-path-demo",
        vec![Evidence::Url { url: DEMO.into() }],
    );
    let page = "Recipe box keeps every recipe you save, with tags and search. ".repeat(40);

    let passed = judge_with(Pages(vec![(DEMO, 200, page)]), &log);
    let ids: Vec<&str> = passed.iter().map(|m| m.milestone_id.as_str()).collect();
    assert_eq!(ids, [milestone("scaffold-runs"), milestone("happy-path-demo")]);
    assert_eq!((passed[0].claimed, passed[0].granted), (None, None));
    assert_eq!(passed[1].claimed, Some(Trust::SelfDeclared));
    assert_eq!(
        passed[1].granted,
        Some(Trust::EvidenceVerified),
        "{:?}",
        passed[1]
    );
    assert!(passed[1].problems.is_empty());

    let failed = judge_with(Pages(vec![(DEMO, 404, String::new())]), &log);
    assert_eq!(failed[1].granted, Some(Trust::SelfDeclared), "{:?}", failed[1]);
    assert!(
        failed[1].problems.iter().any(|p| p.contains("404")),
        "{:?}",
        failed[1]
    );
}

// --- Ingest ------------------------------------------------------------------------------

#[test]
fn audit_p1_ingest_refuses_bundles_that_do_not_verify() {
    let commons = Commons::start();
    let log = plain_run("01RUNREFUSED");
    let mut cases: Vec<(&str, Value)> = Vec::new();

    let mut edited = log.bundle(1);
    edited.events[5]["payload"]["length"] = json!(9000);
    let rehashed = hash::compute_stored_hash(&edited.events[5]);
    edited.events[5]["hash"] = json!(rehashed);
    cases.push((
        "an event edited and re-hashed",
        serde_json::to_value(signed(edited, &log.key)).unwrap(),
    ));

    let stranger = sign::generate();
    cases.push((
        "a bundle signed by a key that signs none of its chains",
        serde_json::to_value(signed(log.bundle(1), &stranger)).unwrap(),
    ));

    let mut altered = log.bundle(1);
    altered.tier = 2;
    cases.push((
        "a bundle changed after signing",
        serde_json::to_value(altered).unwrap(),
    ));

    let unfinished = Log::start("01RUNUNFINISHED").prompt(1);
    cases.push((
        "an unfinished run",
        serde_json::to_value(unfinished.bundle(1)).unwrap(),
    ));

    let mut with_prompts = log.bundle(2);
    with_prompts.tier = 1;
    cases.push((
        "tier 1 with prompt commitments",
        serde_json::to_value(signed(with_prompts, &log.key)).unwrap(),
    ));

    let mut missing_prompt = log.bundle(2);
    missing_prompt.prompts.clear();
    cases.push((
        "tier 2 without a commitment for its prompt",
        serde_json::to_value(signed(missing_prompt, &log.key)).unwrap(),
    ));

    let rooted = Log::start_with_root("01RUNROOTED", Some("/Users/someone/apps/recipes"))
        .prompt(1)
        .finish(30);
    cases.push((
        "a log holding a project root",
        serde_json::to_value(rooted.bundle(1)).unwrap(),
    ));

    let mut format_2 = log.bundle(1);
    format_2.format = 2;
    cases.push((
        "bundle format 2",
        serde_json::to_value(signed(format_2, &log.key)).unwrap(),
    ));

    let head = log.head();
    cases.push((
        "tier 0 naming one device twice",
        serde_json::to_value(log.heads_bundle(vec![head.clone(), head.clone()])).unwrap(),
    ));
    let mut malformed = head.clone();
    malformed.hash = "not a hash".into();
    cases.push((
        "tier 0 with a malformed head hash",
        serde_json::to_value(log.heads_bundle(vec![malformed])).unwrap(),
    ));

    for (case, body) in &cases {
        let (status, answer) = commons.post("/v1/bundles", body);
        assert_eq!(status, 400, "{case}: {answer}");
    }
    assert_eq!(commons.health().tree_size, 0, "a refused bundle added a leaf");
    let (accepted, body) = commons.post("/v1/bundles", &serde_json::to_value(log.bundle(1)).unwrap());
    assert_eq!(accepted, 200, "the untouched bundle: {body}");
    let (anchored, body) = commons.post(
        "/v1/bundles",
        &serde_json::to_value(log.heads_bundle(vec![head])).unwrap(),
    );
    assert_eq!(anchored, 200, "the run's own chain head at tier 0: {body}");
}

#[test]
fn audit_p1_ingest_is_idempotent_on_the_bundle_digest() {
    let commons = Commons::start();
    let bundle = plain_run("01RUNTWICE").bundle(2);
    let first = commons.publish(&bundle);
    let second = commons.publish(&bundle);
    assert_eq!(second.bundle_digest, first.bundle_digest);
    assert_eq!(second.inclusion.leaf_index, first.inclusion.leaf_index);
    assert_eq!(second.judgment, first.judgment);
    assert_eq!(commons.health().tree_size, 1);
}

// --- P1.b: a published run is reproducible from its bundle alone --------------------------

#[test]
fn audit_p1_b_a_published_run_is_reproducible_from_its_bundle_alone() {
    let commons = Commons::start();
    let log = claimed_run("01RUNREPRODUCED", 5, "scaffold-runs", vec![]);
    let receipt = commons.publish(&log.bundle(2));
    let (status, body) = commons.get("/v1/runs/01RUNREPRODUCED");
    assert_eq!(status, 200, "{body}");
    let published: PublishedRun = serde_json::from_value(body).unwrap();
    assert_eq!(published.tier, 2);
    assert_eq!(published.bundle_digest, receipt.bundle_digest);
    assert_eq!(published.judgment, receipt.judgment);
    assert_eq!(published.leaf_index, receipt.inclusion.leaf_index);

    // Everything below uses the published bundle and nothing else.
    let bundle = &published.bundle;
    assert_eq!(bundle::digest(bundle).unwrap(), published.bundle_digest);
    assert!(sign::verify_hash(
        &bundle.publisher_pubkey,
        &published.bundle_digest,
        &bundle.signature
    ));
    hash::verify_stored_chain(&bundle.events).expect("the published chain is intact");
    let events: Vec<Event> = bundle
        .events
        .iter()
        .map(|e| serde_json::from_value(e.clone()).unwrap())
        .collect();
    assert!(sign::verify_event_signatures(&events).ok());
    let last = events.iter().map(|e| e.occurred_at).max().unwrap();
    let view = RunView::fold(&events, last).unwrap();
    assert_eq!(published.view.as_ref(), Some(&view));
    assert_eq!(view.prompt_count, 1);

    let (status, by_digest) = commons.get(&format!("/v1/bundles/{}", receipt.bundle_digest));
    assert_eq!(status, 200, "{by_digest}");
    assert_eq!(by_digest["bundle_digest"], json!(receipt.bundle_digest));
}

// --- P1.c: inclusion proofs check without the commons' code -------------------------------

fn bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd hex {hex}");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
        .collect()
}

fn sha256(data: &[u8]) -> Vec<u8> {
    bytes(&hash::sha256_hex(data))
}

/// RFC 9162 §2.1.3.2, written from the RFC.
fn rfc_9162_verify(leaf: &[u8], index: u64, size: u64, path: &[Vec<u8>], root: &[u8]) -> bool {
    if index >= size {
        return false;
    }
    let (mut f, mut s) = (index, size - 1);
    let mut r = leaf.to_vec();
    for p in path {
        if s == 0 {
            return false;
        }
        if f & 1 == 1 || f == s {
            r = sha256(&[&[1u8][..], p, &r].concat());
            while f & 1 == 0 && f != 0 {
                f >>= 1;
                s >>= 1;
            }
        } else {
            r = sha256(&[&[1u8][..], &r, p].concat());
        }
        f >>= 1;
        s >>= 1;
    }
    s == 0 && r == root
}

#[test]
fn audit_p1_c_inclusion_proofs_verify_in_a_standalone_check() {
    let commons = Commons::start();
    let health = commons.health();
    let receipts: Vec<PublishReceipt> = ["01RUNLEAFA", "01RUNLEAFB", "01RUNLEAFC"]
        .iter()
        .map(|run| commons.publish(&plain_run(run).bundle(1)))
        .collect();
    let indices: Vec<u64> = receipts.iter().map(|r| r.inclusion.leaf_index).collect();
    assert_eq!(indices, [0, 1, 2]);

    let (status, body) = commons.get("/v1/log/head");
    assert_eq!(status, 200, "{body}");
    let head: TreeHead = serde_json::from_value(body).unwrap();
    assert_eq!(head.tree_size, 3);
    assert!(sign::verify_hash(
        &health.commons_pubkey,
        &tree_head_digest(&head).unwrap(),
        &head.signature
    ));

    for receipt in &receipts {
        let leaf_text = format!(
            "{}:{}",
            receipt.bundle_digest,
            judgment_digest(&receipt.judgment).unwrap()
        );
        let leaf = sha256(&[&[0u8][..], leaf_text.as_bytes()].concat());
        let index = receipt.inclusion.leaf_index;
        let (status, body) = commons.get(&format!("/v1/log/proof/{index}?tree_size=3"));
        assert_eq!(status, 200, "{body}");
        let proof: InclusionProof = serde_json::from_value(body).unwrap();
        assert_eq!(bytes(&proof.leaf_hash), leaf, "leaf {index}");
        let path: Vec<Vec<u8>> = proof.audit_path.iter().map(|h| bytes(h)).collect();
        assert!(
            rfc_9162_verify(&leaf, index, 3, &path, &bytes(&head.root_hash)),
            "leaf {index} does not verify against the signed head"
        );
    }
    assert_eq!(commons.get("/v1/log/proof/3?tree_size=3").0, 404);
    assert_eq!(commons.get("/v1/log/proof/0?tree_size=4").0, 400);
}

#[test]
fn audit_p1_c_the_empty_log_has_a_signed_head_of_the_empty_tree() {
    let commons = Commons::start();
    let health = commons.health();
    let (status, body) = commons.get("/v1/log/head");
    assert_eq!(status, 200, "{body}");
    let head: TreeHead = serde_json::from_value(body).unwrap();
    assert_eq!(head.tree_size, 0);
    assert_eq!(head.root_hash, hash::sha256_hex(b""));
    assert!(sign::verify_hash(
        &health.commons_pubkey,
        &tree_head_digest(&head).unwrap(),
        &head.signature
    ));
}
