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
    CommonsHealth, InclusionProof, LINK_FORMAT, Link, LinkClaim, LinkConfirm, LinkStarted, LinkState, Person,
    PersonRecord, PublishReceipt, PublishedRun, TreeHead, judgment_digest, link_claim_digest,
    tree_head_digest,
};
use speedrun_core::{
    Claimant, Event, EventPayload, Evidence, MilestoneOrigin, RunOutcome, RunView, SCHEMA_VERSION, Trust,
    hash, sign,
};

#[path = "support/fake_github.rs"]
mod fake_github;

use fake_github::FakeGitHub;

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

fn fixture_id(name: &str) -> String {
    let key = sign::parse_seed(&hash::sha256_hex(name.as_bytes())).unwrap();
    format!("{name:0<26}.{}", sign::pubkey_hex(&key))
}

impl Log {
    /// A run started at minute 0 with any%'s two milestones proposed and accepted.
    fn start(run_id: &str) -> Self {
        Self::start_with_root(run_id, None)
    }

    fn start_with_root(run_id: &str, project_root: Option<&str>) -> Self {
        let key = sign::parse_seed(&hash::sha256_hex(run_id.as_bytes())).unwrap();
        let mut log = Self {
            run_id: fixture_id(run_id),
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

    /// A milestone the user entered, with `title` as typed.
    fn user_milestone(mut self, minute: i64, milestone_id: &str, title: &str) -> Self {
        self.push(
            minute,
            EventPayload::MilestoneProposed {
                milestone_id: milestone_id.into(),
                origin: MilestoneOrigin::User,
                key: None,
                title: title.into(),
                description: None,
                required_trust: Trust::SelfDeclared,
            },
        );
        self.push(
            minute,
            EventPayload::MilestoneAccepted {
                milestone_id: milestone_id.into(),
                required_trust: None,
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
            rotations: vec![],
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
            rotations: vec![],
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
    home: tempfile::TempDir,
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
        Self::start_with(&[])
    }

    /// A commons with extra environment, such as a GitHub stand-in's.
    fn start_with(env: &[(&str, String)]) -> Self {
        Self::start_in(|| tempfile::tempdir().unwrap(), env)
    }

    /// A commons whose home each attempt takes from `home`, which may hold a database.
    fn start_in(home: impl Fn() -> tempfile::TempDir, env: &[(&str, String)]) -> Self {
        for _ in 0..5 {
            if let Some(c) = Self::try_start(home(), env) {
                return c;
            }
        }
        panic!("speedrun-commons did not start on any of five free ports");
    }

    fn try_start(home: tempfile::TempDir, env: &[(&str, String)]) -> Option<Self> {
        let port = free_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_speedrun-commons"));
        command
            .env("SPEEDRUN_COMMONS_HOME", home.path().join("data"))
            .env("SPEEDRUN_COMMONS_ADDR", format!("127.0.0.1:{port}"))
            .env_remove("SPEEDRUN_GITHUB_CLIENT_ID")
            .env_remove("SPEEDRUN_COMMONS_PUBLIC_URL")
            .env_remove("PORT");
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(home.path().join("stderr")).unwrap())
            .spawn()
            .expect("spawn speedrun-commons");
        let mut commons = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            home,
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

    /// The commons' data directory.
    fn data_dir(&self) -> std::path::PathBuf {
        self.home.path().join("data")
    }

    /// An HTML page: status, `Content-Type`, `Content-Security-Policy`, and body.
    fn page(&self, path: &str) -> Page {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
        let response = match agent.get(&format!("{}{path}", self.base)).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => panic!("GET {path}: {e}"),
        };
        Page {
            status: response.status(),
            content_type: response.header("content-type").unwrap_or_default().to_string(),
            policy: response
                .header("content-security-policy")
                .unwrap_or_default()
                .to_string(),
            body: response.into_string().unwrap(),
        }
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
    assert_eq!(judgment.run_id, log.run_id);
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
    format_2.format = BUNDLE_FORMAT + 1;
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
    let (status, body) = commons.get(&format!("/v1/runs/{}", log.run_id));
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

// --- P1.f: a person links a device through GitHub (COMMONS.md §20) ----------------------

struct Page {
    status: u16,
    content_type: String,
    policy: String,
    body: String,
}

fn commons_with_github(github: &FakeGitHub) -> Commons {
    Commons::start_with(&github.env())
}

/// Start a link for `key` and poll until the GitHub stand-in authorizes it.
fn authorize(commons: &Commons, key: &SigningKey) -> (LinkStarted, Person) {
    let (status, body) = commons.post("/v1/links", &json!({ "device_pubkey": sign::pubkey_hex(key) }));
    assert_eq!(status, 200, "starting a link: {body}");
    let started: LinkStarted = serde_json::from_value(body).unwrap();
    for _ in 0..10 {
        let (status, body) = commons.post(&format!("/v1/links/{}/poll", started.link_id), &json!({}));
        match status {
            202 => continue,
            200 => match serde_json::from_value::<LinkState>(body.clone()).unwrap() {
                LinkState::Authorized { person } => return (started, person),
                LinkState::Pending => panic!("200 for a pending link: {body}"),
            },
            _ => panic!("poll answered {status}: {body}"),
        }
    }
    panic!("the link was never authorized");
}

fn claim_for(commons: &Commons, started: &LinkStarted, person: &Person, key: &SigningKey) -> LinkClaim {
    LinkClaim {
        format: LINK_FORMAT,
        link_id: started.link_id.clone(),
        device_pubkey: sign::pubkey_hex(key),
        github_id: person.github_id,
        login: person.login.clone(),
        commons_pubkey: commons.health().commons_pubkey,
        issued_at: Utc::now(),
    }
}

/// A `LinkConfirm` for `claim`, signed by `signer`.
fn confirmation(claim: &LinkClaim, signer: &SigningKey) -> Value {
    let digest = link_claim_digest(claim).unwrap();
    serde_json::to_value(LinkConfirm {
        claim: claim.clone(),
        device_signature: sign::sign_hash(signer, &digest),
    })
    .unwrap()
}

/// Link `key` to the stand-in's person, asserting every step succeeds.
fn link(commons: &Commons, key: &SigningKey) -> Link {
    let (started, person) = authorize(commons, key);
    let claim = claim_for(commons, &started, &person, key);
    let (status, body) = commons.post(
        &format!("/v1/links/{}/confirm", started.link_id),
        &confirmation(&claim, key),
    );
    assert_eq!(status, 200, "confirming the link: {body}");
    serde_json::from_value(body).unwrap()
}

fn person_record(commons: &Commons, login: &str) -> PersonRecord {
    let (status, body) = commons.get(&format!("/v1/people/{login}"));
    assert_eq!(status, 200, "person {login}: {body}");
    serde_json::from_value(body).unwrap()
}

#[test]
fn audit_p1_f_a_device_links_to_the_github_account_that_authorized_it() {
    let github = FakeGitHub::start(fake_github::Behaviour::default());
    let commons = commons_with_github(&github);
    let key = sign::generate();

    let (status, body) = commons.post("/v1/links", &json!({ "device_pubkey": sign::pubkey_hex(&key) }));
    assert_eq!(status, 200, "{body}");
    let started: LinkStarted = serde_json::from_value(body).unwrap();
    assert_eq!(started.user_code, fake_github::USER_CODE);
    assert_eq!(started.verification_uri, fake_github::VERIFICATION_URI);
    assert_eq!(started.interval_seconds, 5);
    assert!(started.expires_at > Utc::now());

    let poll = format!("/v1/links/{}/poll", started.link_id);
    let (status, body) = commons.post(&poll, &json!({}));
    assert_eq!(status, 202, "{body}");
    assert_eq!(
        serde_json::from_value::<LinkState>(body).unwrap(),
        LinkState::Pending
    );
    let octo = Person {
        github_id: 4242,
        login: "Octo-Person".into(),
    };
    for _ in 0..2 {
        let (status, body) = commons.post(&poll, &json!({}));
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_value::<LinkState>(body).unwrap(),
            LinkState::Authorized { person: octo.clone() }
        );
    }
    assert_eq!(
        github.requests_to("/login/oauth/access_token").len(),
        2,
        "an authorized link asked GitHub again"
    );

    let claim = claim_for(&commons, &started, &octo, &key);
    let confirm = format!("/v1/links/{}/confirm", started.link_id);
    let (status, body) = commons.post(&confirm, &confirmation(&claim, &key));
    assert_eq!(status, 200, "{body}");
    let link: Link = serde_json::from_value(body).unwrap();
    assert_eq!(link.claim, claim);
    let digest = link_claim_digest(&claim).unwrap();
    assert!(sign::verify_hash(
        &sign::pubkey_hex(&key),
        &digest,
        &link.device_signature
    ));
    assert!(sign::verify_hash(
        &commons.health().commons_pubkey,
        &digest,
        &link.commons_signature
    ));
    let (status, again) = commons.post(&confirm, &confirmation(&claim, &key));
    assert_eq!(status, 200, "{again}");
    assert_eq!(serde_json::from_value::<Link>(again).unwrap(), link);

    let record = person_record(&commons, "octo-person");
    assert_eq!(record.person, octo);
    assert_eq!(record.links, vec![link]);
    assert!(record.publications.is_empty());

    let device_code = &github.requests_to("/login/device/code")[0];
    assert!(
        device_code.body.contains(fake_github::CLIENT_ID),
        "{}",
        device_code.body
    );
    let user = &github.requests_to("/user")[0];
    assert_eq!(
        user.header("authorization"),
        Some(format!("Bearer {}", fake_github::ACCESS_TOKEN).as_str())
    );
    for request in github.requests() {
        assert_eq!(
            request.header("user-agent"),
            Some("speedrun-commons"),
            "{request:?}"
        );
        assert!(
            request.headers.iter().all(|(_, v)| !v.contains('@')),
            "a GitHub request carried an address: {request:?}"
        );
    }
}

#[test]
fn audit_p1_f_a_link_needs_the_devices_signature_and_the_authorized_person() {
    let github = FakeGitHub::start(fake_github::Behaviour::default());
    let commons = commons_with_github(&github);
    let key = sign::generate();
    let stranger = sign::generate();

    let (status, _) = commons.post("/v1/links", &json!({ "device_pubkey": "not a key" }));
    assert_eq!(status, 400, "a malformed device key");
    assert_eq!(commons.post("/v1/links/01UNKNOWNLINK/poll", &json!({})).0, 404);

    // Confirming before GitHub authorized anyone.
    let (status, body) = commons.post("/v1/links", &json!({ "device_pubkey": sign::pubkey_hex(&key) }));
    assert_eq!(status, 200, "{body}");
    let early: LinkStarted = serde_json::from_value(body).unwrap();
    let nobody = Person {
        github_id: 4242,
        login: "Octo-Person".into(),
    };
    let premature = claim_for(&commons, &early, &nobody, &key);
    let (status, body) = commons.post(
        &format!("/v1/links/{}/confirm", early.link_id),
        &confirmation(&premature, &key),
    );
    assert_eq!(status, 409, "confirmed before authorization: {body}");

    let (started, person) = authorize(&commons, &key);
    let good = claim_for(&commons, &started, &person, &key);
    let confirm = format!("/v1/links/{}/confirm", started.link_id);
    let mut cases: Vec<(&str, Value)> = vec![("signed by another key", confirmation(&good, &stranger))];
    let mut other_login = good.clone();
    other_login.login = "someone-else".into();
    cases.push(("another login", confirmation(&other_login, &key)));
    let mut other_id = good.clone();
    other_id.github_id = 7;
    cases.push(("another GitHub id", confirmation(&other_id, &key)));
    let mut other_commons = good.clone();
    other_commons.commons_pubkey = sign::pubkey_hex(&stranger);
    cases.push(("another commons", confirmation(&other_commons, &key)));
    let mut other_link = good.clone();
    other_link.link_id = early.link_id.clone();
    cases.push(("another link's id", confirmation(&other_link, &key)));
    let mut other_device = good.clone();
    other_device.device_pubkey = sign::pubkey_hex(&stranger);
    cases.push((
        "a device the link did not start with",
        confirmation(&other_device, &stranger),
    ));
    let mut format_2 = good.clone();
    format_2.format = BUNDLE_FORMAT + 1;
    cases.push(("link format 2", confirmation(&format_2, &key)));
    for (case, body) in &cases {
        let (status, answer) = commons.post(&confirm, body);
        assert_eq!(status, 400, "{case}: {answer}");
    }
    assert_eq!(
        commons.get("/v1/people/octo-person").0,
        404,
        "a refused confirmation linked someone"
    );
    assert_eq!(
        commons
            .post("/v1/links/01UNKNOWNLINK/confirm", &confirmation(&good, &key))
            .0,
        404
    );
}

#[test]
fn audit_p1_f_a_refused_authorization_links_nothing() {
    let github = FakeGitHub::start(fake_github::Behaviour {
        deny: true,
        ..Default::default()
    });
    let commons = commons_with_github(&github);
    let key = sign::generate();
    let (status, body) = commons.post("/v1/links", &json!({ "device_pubkey": sign::pubkey_hex(&key) }));
    assert_eq!(status, 200, "{body}");
    let started: LinkStarted = serde_json::from_value(body).unwrap();
    let poll = format!("/v1/links/{}/poll", started.link_id);
    assert_eq!(commons.post(&poll, &json!({})).0, 410);
    assert_eq!(commons.post(&poll, &json!({})).0, 410, "a refused link came back");
    let person = Person {
        github_id: 4242,
        login: "Octo-Person".into(),
    };
    let claim = claim_for(&commons, &started, &person, &key);
    let (status, _) = commons.post(
        &format!("/v1/links/{}/confirm", started.link_id),
        &confirmation(&claim, &key),
    );
    assert_eq!(status, 410);
    assert_eq!(commons.get("/v1/people/octo-person").0, 404);

    let unconfigured = Commons::start();
    let (status, body) = unconfigured.post("/v1/links", &json!({ "device_pubkey": sign::pubkey_hex(&key) }));
    assert_eq!(status, 503, "linking without a GitHub client id: {body}");
}

#[test]
fn audit_p1_f_the_commons_keeps_no_github_token() {
    let github = FakeGitHub::start(fake_github::Behaviour::default());
    let commons = commons_with_github(&github);
    link(&commons, &sign::generate());
    let mut files = 0;
    for entry in std::fs::read_dir(commons.data_dir()).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        files += 1;
        assert!(
            !bytes
                .windows(fake_github::ACCESS_TOKEN.len())
                .any(|w| w == fake_github::ACCESS_TOKEN.as_bytes()),
            "{} holds the GitHub token",
            path.display()
        );
    }
    assert!(files >= 2, "the commons wrote no files");
}

#[test]
fn audit_p1_f_a_persons_publications_are_those_their_devices_signed() {
    let github = FakeGitHub::start(fake_github::Behaviour::default());
    let commons = commons_with_github(&github);
    let mine = plain_run("01RUNMINE");
    let theirs = plain_run("01RUNTHEIRS");
    link(&commons, &mine.key);
    let first = commons.publish(&mine.bundle(1));
    commons.publish(&theirs.bundle(1));
    let second = commons.publish(&mine.heads_bundle(vec![mine.head()]));

    let record = person_record(&commons, "OCTO-PERSON");
    let digests: Vec<&str> = record
        .publications
        .iter()
        .map(|p| p.bundle_digest.as_str())
        .collect();
    assert_eq!(
        digests,
        [second.bundle_digest.as_str(), first.bundle_digest.as_str()]
    );
    assert!(record.publications.iter().all(|p| p.run_id == mine.run_id));
}

#[test]
fn guard_p1_f_a_login_names_the_person_who_last_authorized_with_it() {
    let github = FakeGitHub::start(fake_github::Behaviour {
        user_id: 1,
        login: "alice",
        ..Default::default()
    });
    let commons = commons_with_github(&github);
    let before = plain_run("01RUNFORMERALICE");
    link(&commons, &before.key);
    commons.publish(&before.bundle(1));

    // The first alice renamed; a second account took the name and linked.
    github.set_user(2, "Alice");
    let after = plain_run("01RUNNEWALICE");
    link(&commons, &after.key);
    commons.publish(&after.bundle(1));

    let record = person_record(&commons, "alice");
    assert_eq!(record.person.github_id, 2);
    let runs: Vec<&str> = record.publications.iter().map(|p| p.run_id.as_str()).collect();
    assert_eq!(runs, [after.run_id.as_str()]);

    let former = commons.page(&format!("/runs/{}", before.run_id));
    assert!(former.body.contains("github id 1"), "{}", former.body);
    assert!(
        !former.body.contains("href=\"/people/"),
        "the former alice links to a person page"
    );
    let front = commons.page("/");
    assert!(front.body.contains("github id 1"), "{}", front.body);
}

// --- P1.g: the public pages (COMMONS.md §20) -------------------------------------------------

fn hms(ms: i64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
}

fn published(commons: &Commons, run_id: &str) -> PublishedRun {
    let (status, body) = commons.get(&format!("/v1/runs/{run_id}"));
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body).unwrap()
}

#[test]
fn audit_p1_g_the_run_page_shows_the_published_record() {
    let github = FakeGitHub::start(fake_github::Behaviour::default());
    let commons = commons_with_github(&github);
    let log = Log::start("01RUNPAGE")
        .prompt(1)
        .claim(10, "scaffold-runs", vec![])
        .finish(40);
    link(&commons, &log.key);
    let receipt = commons.publish(&log.bundle(1));

    let page = commons.page(&format!("/runs/{}", log.run_id));
    assert_eq!(page.status, 200);
    assert!(
        page.content_type.starts_with("text/html"),
        "{}",
        page.content_type
    );
    let run = published(&commons, &log.run_id);
    let timing = &run.view.as_ref().unwrap().timing;
    let (_, head) = commons.get("/v1/log/head");
    let head: TreeHead = serde_json::from_value(head).unwrap();
    for expected in [
        log.run_id.clone(),
        "any-percent@1".into(),
        "Octo-Person".into(),
        "href=\"/people/Octo-Person\"".into(),
        "scaffold-runs".into(),
        "happy-path-demo".into(),
        "self_declared".into(),
        "evidence_verified".into(),
        hms(timing.wall_ms),
        hms(timing.agent_ms),
        hms(timing.human_ms),
        receipt.bundle_digest.clone(),
        format!("href=\"/v1/bundles/{}\"", receipt.bundle_digest),
        receipt.judgment.signature.clone(),
        commons.health().commons_pubkey,
        head.root_hash.clone(),
        "curl".into(),
    ] {
        assert!(
            page.body.contains(&expected),
            "the run page lacks {expected:?}:\n{}",
            page.body
        );
    }
}

#[test]
fn audit_p1_g_pages_escape_what_the_log_says() {
    let commons = Commons::start();
    let log = Log::start("01RUNESCAPE")
        .prompt(1)
        .user_milestone(
            2,
            "01KUSERMILESTONE000000000",
            "<script>alert(\"x\")</script> & <b>bold</b>",
        )
        .finish(30);
    commons.publish(&log.bundle(1));
    let page = commons.page(&format!("/runs/{}", log.run_id));
    assert_eq!(page.status, 200);
    assert!(
        !page.body.contains("<script"),
        "a script tag reached the page:\n{}",
        page.body
    );
    assert!(
        !page.body.contains("<b>bold"),
        "markup from the log reached the page"
    );
    assert!(
        page.body.contains("&lt;script&gt;"),
        "the title is missing:\n{}",
        page.body
    );
    assert!(page.body.contains("&amp;"));
}

#[test]
fn audit_p1_g_the_front_page_lists_publications_newest_first() {
    let github = FakeGitHub::start(fake_github::Behaviour::default());
    let commons = commons_with_github(&github);
    let linked = plain_run("01RUNLISTONE");
    link(&commons, &linked.key);
    let unlinked = plain_run("01RUNLISTTWO");
    commons.publish(&linked.bundle(1));
    commons.publish(&unlinked.bundle(2));
    commons.publish(&plain_run("01RUNLISTTHREE").bundle(1));

    let page = commons.page("/");
    assert_eq!(page.status, 200);
    let at = |text: &str| {
        page.body
            .find(text)
            .unwrap_or_else(|| panic!("the front page lacks {text:?}:\n{}", page.body))
    };
    assert!(
        at(&format!("href=\"/runs/{}\"", fixture_id("01RUNLISTTHREE")))
            < at(&format!("href=\"/runs/{}\"", unlinked.run_id))
    );
    assert!(
        at(&format!("href=\"/runs/{}\"", unlinked.run_id)) < at(&format!("href=\"/runs/{}\"", linked.run_id))
    );
    at("href=\"/people/Octo-Person\"");
    at("unlinked device");
    at(&sign::fingerprint(&sign::pubkey_hex(&unlinked.key)).unwrap());
}

#[test]
fn audit_p1_g_pages_carry_the_security_policy_and_fit_a_phone() {
    let commons = Commons::start();
    commons.publish(&plain_run("01RUNPOLICY").bundle(1));
    for path in [
        "/",
        &format!("/runs/{}", fixture_id("01RUNPOLICY")),
        "/runs/01RUNNOSUCHRUN",
        "/people/nobody-here",
    ] {
        let page = commons.page(path);
        assert!(
            page.content_type.starts_with("text/html"),
            "{path}: {}",
            page.content_type
        );
        assert_eq!(
            page.policy, "default-src 'none'; style-src 'unsafe-inline'",
            "{path}"
        );
        assert!(
            page.body.contains("name=\"viewport\""),
            "{path} has no viewport tag"
        );
        assert!(!page.body.contains("<script"), "{path} carries a script");
    }
    assert_eq!(commons.page("/runs/01RUNNOSUCHRUN").status, 404);
    assert_eq!(commons.page("/people/nobody-here").status, 404);
}

#[test]
fn audit_p1_g_a_tier_0_page_shows_chain_heads_only() {
    let commons = Commons::start();
    let log = plain_run("01RUNHEADSONLY");
    let head = log.head();
    commons.publish(&log.heads_bundle(vec![head.clone()]));
    let page = commons.page(&format!("/runs/{}", log.run_id));
    assert_eq!(page.status, 200);
    assert!(page.body.contains(&head.hash), "{}", page.body);
    assert!(page.body.contains(&head.device_id));
    assert!(
        !page.body.contains("happy-path-demo"),
        "a tier 0 page showed milestones"
    );
}

#[test]
fn guard_p1_g_curl_commands_name_the_configured_address_never_the_host_header() {
    let fetch = |commons: &Commons| {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build();
        agent
            .get(&format!("{}/runs/{}", commons.base, fixture_id("01RUNCURL")))
            .set("Host", "evil.example")
            .call()
            .unwrap()
            .into_string()
            .unwrap()
    };
    let plain = Commons::start();
    let digest = plain.publish(&plain_run("01RUNCURL").bundle(1)).bundle_digest;
    let body = fetch(&plain);
    assert!(
        body.contains(&format!("curl $COMMONS/v1/bundles/{digest}")),
        "{body}"
    );
    assert!(!body.contains("evil.example"));

    let named = Commons::start_with(&[(
        "SPEEDRUN_COMMONS_PUBLIC_URL",
        "https://commons.example/".to_string(),
    )]);
    let digest = named.publish(&plain_run("01RUNCURL").bundle(1)).bundle_digest;
    let body = fetch(&named);
    assert!(
        body.contains(&format!("curl https://commons.example/v1/bundles/{digest}")),
        "{body}"
    );
    assert!(!body.contains("evil.example"));
}

// --- P1.h: running the commons on a host (COMMONS.md §20) ------------------------------------

#[test]
fn audit_p1_h_the_commons_listens_on_port_when_no_address_is_set() {
    let home = tempfile::tempdir().unwrap();
    let port = free_port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_speedrun-commons"))
        .env("SPEEDRUN_COMMONS_HOME", home.path().join("data"))
        .env_remove("SPEEDRUN_COMMONS_ADDR")
        .env("PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + STARTUP_LIMIT;
    let mut status = 0;
    while Instant::now() < deadline && status != 200 {
        status = request("GET", &format!("http://127.0.0.1:{port}/v1/health"), None).0;
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(status, 200, "nothing answered on PORT {port}");
}

#[test]
fn audit_p1_h_a_backup_restores_the_log() {
    let commons = Commons::start();
    commons.publish(&plain_run("01RUNBACKUPONE").bundle(1));
    commons.publish(&plain_run("01RUNBACKUPTWO").bundle(2));
    let (_, head) = commons.get("/v1/log/head");
    let head: TreeHead = serde_json::from_value(head).unwrap();

    let out = tempfile::tempdir().unwrap();
    let copy = out.path().join("copy.sqlite");
    let backup = || {
        Command::new(env!("CARGO_BIN_EXE_speedrun-commons"))
            .arg("backup")
            .arg(&copy)
            .env("SPEEDRUN_COMMONS_HOME", commons.data_dir())
            .output()
            .unwrap()
    };
    let first = backup();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&first.stdout).trim(),
        format!("backed up 2 leaves to {}", copy.display())
    );
    assert_eq!(backup().status.code(), Some(1), "the backup overwrote a file");

    let key = std::fs::read(commons.data_dir().join("commons.key")).unwrap();
    let copied = std::fs::read(&copy).unwrap();
    let restored = Commons::start_in(
        || {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join("data")).unwrap();
            std::fs::write(home.path().join("data/commons.sqlite"), &copied).unwrap();
            std::fs::write(home.path().join("data/commons.key"), &key).unwrap();
            home
        },
        &[],
    );
    assert_eq!(restored.health().tree_size, 2);
    assert_eq!(restored.health().commons_pubkey, commons.health().commons_pubkey);
    let (_, again) = restored.get("/v1/log/head");
    let again: TreeHead = serde_json::from_value(again).unwrap();
    assert_eq!(again.root_hash, head.root_hash);
    assert_eq!(published(&restored, &fixture_id("01RUNBACKUPTWO")).tier, 2);
}

#[test]
fn publication_identity_rejects_replacement_and_keeps_history() {
    let commons = Commons::start();
    let original = plain_run("IDENTITY-ORIGINAL");
    let first = commons.publish(&original.bundle(1));
    let attacker = sign::generate();
    let forged_head = signed(original.heads_bundle(vec![original.head()]), &attacker);
    let (status, _) = commons.post("/v1/bundles", &serde_json::to_value(forged_head).unwrap());
    assert_eq!(status, 400);

    let mut forged = plain_run("IDENTITY-ATTACKER");
    forged.run_id = original.run_id.clone();
    let mut previous = None;
    for event in &mut forged.events {
        event.run_id = original.run_id.clone();
        event.prev_hash = previous;
        event.hash = hash::compute_hash(event);
        event.sig = Some(sign::sign_hash(&forged.key, &event.hash));
        previous = Some(event.hash.clone());
    }
    let (status, _) = commons.post("/v1/bundles", &serde_json::to_value(forged.bundle(1)).unwrap());
    assert_eq!(status, 400);
    let (status, body) = commons.get(&format!("/v1/runs/{}", original.run_id));
    assert_eq!(status, 200);
    assert_eq!(body["bundle_digest"], first.bundle_digest);
    assert!(
        commons
            .page(&format!("/runs/{}", original.run_id))
            .body
            .contains(&first.bundle_digest)
    );
    let second = commons.publish(&original.heads_bundle(vec![original.head()]));
    let (status, history) = commons.get(&format!("/v1/runs/{}/history", original.run_id));
    assert_eq!(status, 200);
    assert_eq!(history[0]["bundle_digest"], first.bundle_digest);
    assert_eq!(history[1]["bundle_digest"], second.bundle_digest);
    assert_eq!(commons.health().tree_size, 2);
}

#[test]
fn publication_identity_allows_certified_rotation_and_refuses_rollback() {
    let commons = Commons::start();
    let mut log = plain_run("IDENTITY-ROTATION");
    commons.publish(&log.bundle(1));
    let old = log.key.clone();
    let next = sign::generate();
    let next_key = sign::pubkey_hex(&next);
    let cert = sign::rotation_certificate(&old, &next_key);
    let device = log.events[0].device_id.clone();
    log.key = next;
    log.push(
        50,
        EventPayload::DeviceRotated {
            new_pubkey: next_key.clone(),
            certificate: cert.clone(),
        },
    );
    let last = log.events.last_mut().unwrap();
    last.device_id = device;
    last.hash = hash::compute_hash(last);
    last.sig = Some(sign::sign_hash(&log.key, &last.hash));
    let proof = speedrun_core::bundle::RotationProof {
        new_pubkey: next_key,
        certificate: cert,
    };
    let mut rotated = log.bundle(1);
    rotated.rotations.push(proof.clone());
    let rotated = signed(rotated, &log.key);
    commons.publish(&rotated);
    let mut heads = log.heads_bundle(vec![log.head()]);
    heads.rotations.push(proof);
    commons.publish(&signed(heads, &log.key));
    let rollback = signed(log.heads_bundle(vec![log.head()]), &old);
    let (status, _) = commons.post("/v1/bundles", &serde_json::to_value(rollback).unwrap());
    assert_eq!(status, 400);
    assert_eq!(commons.health().tree_size, 3);
}
