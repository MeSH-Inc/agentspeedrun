use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use chrono::Utc;
use ed25519_dalek::SigningKey;
use speedrun_commons::{
    record::{self, PublishError},
    store::Store,
};
use speedrun_core::{
    Claimant, Event, EventPayload, Evidence, MilestoneOrigin, RunOutcome, SCHEMA_VERSION, Trust,
    bundle::{self, BUNDLE_FORMAT, Bundle, ChainHead},
    hash, sign,
};
use speedrun_verify::{Fetch, NoSecrets, Page, Verifier};

struct Gate {
    entered: AtomicUsize,
    active: AtomicUsize,
    maximum: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn new() -> Self {
        Self {
            entered: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            open: Mutex::new(false),
            changed: Condvar::new(),
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct SlowFetch(Arc<Gate>);
impl Fetch for SlowFetch {
    fn get(&self, url: &str, _: &[(&str, &str)], _: bool) -> Result<Page, String> {
        let now = self.0.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.0.maximum.fetch_max(now, Ordering::SeqCst);
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        let mut open = self.0.open.lock().unwrap();
        while !*open {
            open = self.0.changed.wait(open).unwrap();
        }
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        Ok(Page {
            status: 200,
            url: url.into(),
            body: "a functioning service with a real page".into(),
        })
    }
}

fn fixture(seed: u8) -> (Bundle, Bundle) {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let pubkey = sign::pubkey_hex(&key);
    let run_id = format!("{}.{}", ulid::Ulid::from_bytes([seed; 16]), pubkey);
    let id = sign::fingerprint(&pubkey).unwrap();
    let now = Utc::now();
    let payloads = vec![
        EventPayload::RunStarted {
            project_id: "project".into(),
            project_root: None,
            category_id: "any-percent".into(),
            category_version: 1,
            device_pubkey: Some(pubkey.clone()),
            start_head: None,
            start_head_committed_at: None,
            ruleset_digest: None,
            disclosure_commitment: None,
        },
        EventPayload::MilestoneProposed {
            milestone_id: "m".into(),
            origin: MilestoneOrigin::Predefined,
            key: Some("happy-path-demo".into()),
            title: "demo".into(),
            description: None,
            required_trust: Trust::EvidenceVerified,
        },
        EventPayload::MilestoneAccepted {
            milestone_id: "m".into(),
            required_trust: None,
        },
        EventPayload::MilestoneReached {
            milestone_id: "m".into(),
            trust: Trust::SelfDeclared,
            evidence: vec![Evidence::Url {
                url: "https://example.com/".into(),
            }],
            claimant: Claimant::User,
        },
        EventPayload::RunFinished {
            outcome: RunOutcome::Abandoned,
            reason: None,
        },
    ];
    let mut events: Vec<Event> = Vec::new();
    for payload in payloads {
        let seq = events.len() as u64 + 1;
        let mut e = Event {
            event_id: format!("{run_id}:{seq}"),
            schema_version: SCHEMA_VERSION,
            run_id: run_id.clone(),
            seq,
            device_id: id.clone(),
            occurred_at: now,
            recorded_at: now,
            payload,
            prev_hash: events.last().map(|e| e.hash.clone()),
            hash: String::new(),
            sig: None,
        };
        e.hash = hash::compute_hash(&e);
        e.sig = Some(sign::sign_hash(&key, &e.hash));
        events.push(e);
    }
    let last = events.last().unwrap();
    let head = ChainHead {
        device_id: id,
        seq: last.seq,
        hash: last.hash.clone(),
    };
    let base = Bundle {
        format: BUNDLE_FORMAT,
        run_id,
        tier: 1,
        events: events.iter().map(|e| serde_json::to_value(e).unwrap()).collect(),
        chain_heads: vec![],
        prompts: vec![],
        rotations: vec![],
        publisher_pubkey: pubkey,
        signature: String::new(),
    };
    let signed = |mut b: Bundle| {
        b.signature = sign::sign_hash(&key, &bundle::digest(&b).unwrap());
        b
    };
    let mut anchor = base.clone();
    anchor.tier = 0;
    anchor.events.clear();
    anchor.chain_heads = vec![head];
    (signed(base), signed(anchor))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn slow_checks_are_bounded_and_append_order_remains_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("commons.sqlite")).await.unwrap());
    let key = Arc::new(SigningKey::from_bytes(&[99; 32]));
    let gate = Arc::new(Gate::new());
    let verifier = Arc::new(Verifier {
        fetch: Box::new(SlowFetch(gate.clone())),
        secrets: Box::new(NoSecrets),
    });
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let bundles: Vec<_> = (1..=5).map(fixture).collect();
    let mut pending = Vec::new();
    for (bundle, _) in bundles.iter().take(4) {
        let (store, key, verifier, lock, bundle) = (
            store.clone(),
            key.clone(),
            verifier.clone(),
            lock.clone(),
            bundle.clone(),
        );
        pending.push(tokio::spawn(async move {
            let body = serde_json::to_string(&bundle).unwrap();
            record::publish(&store, &key, &verifier, &lock, bundle, body).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate.entered.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("four checks start");
    let fifth = bundles[4].0.clone();
    let body = serde_json::to_string(&fifth).unwrap();
    assert!(matches!(
        record::publish(&store, &key, &verifier, &lock, fifth, body).await,
        Err(PublishError::Busy)
    ));
    let anchor = bundles[0].1.clone();
    let body = serde_json::to_string(&anchor).unwrap();
    let first = tokio::time::timeout(
        Duration::from_secs(2),
        record::publish(&store, &key, &verifier, &lock, anchor.clone(), body.clone()),
    )
    .await
    .expect("independent anchor is not blocked")
    .unwrap();
    let duplicate = record::publish(&store, &key, &verifier, &lock, anchor, body)
        .await
        .unwrap();
    assert_eq!(first.inclusion.leaf_index, duplicate.inclusion.leaf_index);
    assert_eq!(store.tree_size().await.unwrap(), 1);
    gate.release();
    let mut receipts = Vec::new();
    for task in pending {
        receipts.push(task.await.unwrap().unwrap());
    }
    assert_eq!(gate.maximum.load(Ordering::SeqCst), 4);
    assert_eq!(store.tree_size().await.unwrap(), 5);
    let run_id = &bundles[0].0.run_id;
    let history = store.publication_history(run_id).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].leaf_index, 0);
    assert!(history[1].leaf_index > history[0].leaf_index);
    assert!(history[1].received_at >= history[0].received_at);
    assert_eq!(
        store
            .latest_publication_for_run(run_id)
            .await
            .unwrap()
            .unwrap()
            .leaf_index,
        history[1].leaf_index
    );
    // Wall clocks can move backwards; listing order must still follow leaves.
    use sqlx::Connection;
    let mut db = sqlx::SqliteConnection::connect(&format!(
        "sqlite://{}",
        dir.path().join("commons.sqlite").display()
    ))
    .await
    .unwrap();
    // Simulate an old database written while its wall clock moved backwards.
    sqlx::query("DROP TRIGGER bundles_no_update")
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE bundles SET received_at = '2099-01-01T00:00:00Z' WHERE digest = ?")
        .bind(&history[0].digest)
        .execute(&mut db)
        .await
        .unwrap();
    let listed = store.recent_publications(10).await.unwrap();
    assert!(
        listed
            .windows(2)
            .all(|pair| pair[0].summary.leaf_index > pair[1].summary.leaf_index)
    );
    let mut indices: Vec<_> = receipts.iter().map(|r| r.inclusion.leaf_index).collect();
    indices.sort();
    indices.dedup();
    assert_eq!(indices.len(), 4);

    // Duplicate requests may both finish network work, but the final append
    // boundary must return the same leaf for both.
    let gate2 = Arc::new(Gate::new());
    let verifier2 = Arc::new(Verifier {
        fetch: Box::new(SlowFetch(gate2.clone())),
        secrets: Box::new(NoSecrets),
    });
    let bundle = fixture(7).0;
    let mut duplicates = Vec::new();
    for _ in 0..2 {
        let (store, key, verifier, lock, bundle) = (
            store.clone(),
            key.clone(),
            verifier2.clone(),
            lock.clone(),
            bundle.clone(),
        );
        duplicates.push(tokio::spawn(async move {
            let body = serde_json::to_string(&bundle).unwrap();
            record::publish(&store, &key, &verifier, &lock, bundle, body).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate2.entered.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("duplicate checks start");
    gate2.release();
    let first = duplicates.remove(0).await.unwrap().unwrap();
    let second = duplicates.remove(0).await.unwrap().unwrap();
    assert_eq!(first.inclusion.leaf_index, second.inclusion.leaf_index);
    assert_eq!(store.tree_size().await.unwrap(), 6);

    // A newly anchored incompatible head must be noticed after slow checking.
    let gate3 = Arc::new(Gate::new());
    let verifier3 = Arc::new(Verifier {
        fetch: Box::new(SlowFetch(gate3.clone())),
        secrets: Box::new(NoSecrets),
    });
    let (disclosed, mut conflicting_anchor) = fixture(8);
    let (store2, key2, verifier2, lock2) = (store.clone(), key.clone(), verifier3.clone(), lock.clone());
    let pending = tokio::spawn(async move {
        let body = serde_json::to_string(&disclosed).unwrap();
        record::publish(&store2, &key2, &verifier2, &lock2, disclosed, body).await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate3.entered.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    conflicting_anchor.chain_heads[0].hash = "ab".repeat(32);
    let origin = SigningKey::from_bytes(&[8; 32]);
    conflicting_anchor.signature = sign::sign_hash(&origin, &bundle::digest(&conflicting_anchor).unwrap());
    let body = serde_json::to_string(&conflicting_anchor).unwrap();
    record::publish(&store, &key, &verifier3, &lock, conflicting_anchor, body)
        .await
        .unwrap();
    gate3.release();
    assert!(matches!(pending.await.unwrap(), Err(PublishError::Refused(_))));
    assert_eq!(store.tree_size().await.unwrap(), 7);
}
