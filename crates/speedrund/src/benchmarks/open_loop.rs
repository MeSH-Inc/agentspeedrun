//! Bounded, fixed-rate pressure with multiple active projects.
use super::*;
use std::time::Duration as StdDuration;
use tokio::{sync::Semaphore, task::JoinSet};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual performance benchmark"]
async fn open_loop_pressure() {
    let value = |key: &str, default: &str| -> usize {
        std::env::var(key)
            .unwrap_or_else(|_| default.into())
            .parse()
            .unwrap()
    };
    let count = value("BENCH_EVENTS", "1000");
    let projects = value("BENCH_PROJECTS", "3");
    let subscribers = value("BENCH_SUBSCRIBERS", "5");
    let iterations = value("BENCH_ITERATIONS", "30");
    let rate = value("BENCH_RATE", "20");
    let max_inflight = value("BENCH_MAX_INFLIGHT", "16");
    assert!(projects > 0 && subscribers > 0 && iterations > 0 && rate > 0 && max_inflight > 0);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("speedrun.sqlite");
    let mut store = Store::open(&db, &dir.path().join("device.key")).unwrap();
    for project in 0..projects {
        history::seed_project(&mut store, count, project).unwrap();
    }
    let service = Service::new(store, category::builtin().unwrap(), "benchmark".into());
    let service = Arc::new(Mutex::new(service));
    let state = AppState {
        service: service.clone(),
        token: Arc::new("benchmark".into()),
        display_token: Arc::new("benchmark".into()),
        agent_token: Arc::new("benchmark-agent".into()),
        health: Arc::new(Health::resolve(dir.path()).unwrap()),
        verifier: Arc::new(speedrun_verify::Verifier::default()),
        changes: Arc::new(watch::channel(0).0),
        shutdown: Shutdown::install().unwrap(),
        live_hub: Arc::default(),
    };
    state.service.lock().unwrap().live(Duration::hours(24)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = state.shutdown.clone();
    let server = tokio::spawn(async move { axum::serve(listener, http::router(state, &[])).await.unwrap() });
    let _server_guard = ServerGuard {
        shutdown: shutdown.clone(),
        abort: server.abort_handle(),
    };
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut readers = Vec::new();
    for id in 0..subscribers {
        let tx = tx.clone();
        readers.push(tokio::task::spawn_blocking(move || subscribe(addr, id, tx)));
    }
    drop(tx);
    wait_cursor(&mut rx, subscribers, (count * projects) as u64).await;
    let permits = Arc::new(Semaphore::new(max_inflight));
    let mut writers = JoinSet::new();
    let start = Instant::now();
    let mut rejected = 0usize;
    let mut max_observed_inflight = 0usize;
    for i in 0..iterations {
        let scheduled = start + StdDuration::from_secs_f64(i as f64 / rate as f64);
        tokio::time::sleep_until(scheduled.into()).await;
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            rejected += 1;
            continue;
        };
        max_observed_inflight = max_observed_inflight.max(max_inflight - permits.available_permits());
        let run = if i % projects == 0 {
            "benchmark-run".to_string()
        } else {
            format!("benchmark-run-{}", i % projects)
        };
        let body = serde_json::to_string(&AppendRequest {
            event_id: format!("open-loop-{i}"),
            run_id: run,
            occurred_at: None,
            payload: EventPayload::PromptSubmitted {
                session_id: format!("open-{i}"),
                host: "benchmark".into(),
                model: None,
                content_digest: String::new(),
                length: 80,
                redacted: false,
            },
        })
        .unwrap();
        writers.spawn(async move {
            let _permit = permit;
            tokio::time::timeout(
                StdDuration::from_secs(120),
                request(addr, "POST", "/v1/events", &body),
            )
            .await
            .map(|_| (i, scheduled, Instant::now()))
        });
    }
    tokio::time::sleep_until((start + StdDuration::from_secs_f64(iterations as f64 / rate as f64)).into())
        .await;
    let offered_end = Instant::now();
    let mut completed = Vec::new();
    let mut failures = 0usize;
    while let Some(result) = writers.join_next().await {
        match result {
            Ok(Ok(sample)) => completed.push(sample),
            _ => failures += 1,
        }
    }
    let end = Instant::now();
    let target = (count * projects + completed.len()) as u64;
    // Keep every observation: a later snapshot also acknowledges skipped revisions.
    let mut observations = Vec::new();
    let mut seen = vec![false; subscribers];
    let deadline = tokio::time::Instant::now() + StdDuration::from_secs(120);
    while seen.iter().any(|x| !x) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((id, cursor, at))) => {
                if cursor >= target {
                    seen[id] = true;
                }
                observations.push((id, cursor, at));
            }
            _ => {
                failures += 1;
                break;
            }
        }
    }
    shutdown.request();
    server.abort();
    for reader in readers {
        let _ = tokio::time::timeout(StdDuration::from_secs(10), reader).await;
    }
    let elapsed = end.duration_since(start).as_secs_f64();
    let offered_seconds = offered_end.duration_since(start).as_secs_f64();
    let drain_seconds = end.duration_since(offered_end).as_secs_f64();
    println!(
        "open_loop projects={projects} events_per_project={count} subscribers={subscribers} offered={} admitted={} completed={} admission_drops={} failures={} target_per_second={rate} actual_offered_per_second={:.3} completed_per_second_including_drain={:.3} offered_seconds={offered_seconds:.3} drain_seconds={drain_seconds:.3} elapsed_seconds={elapsed:.3} max_inflight={max_inflight} max_observed_inflight={max_observed_inflight}",
        iterations,
        iterations - rejected,
        completed.len(),
        rejected,
        failures,
        iterations as f64 / offered_seconds,
        completed.len() as f64 / elapsed
    );
    assert_eq!(failures, 0, "writer or subscriber timeout/failure");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mut responses = Vec::new();
    let mut delivery = Vec::new();
    let mut coalesced = 0usize;
    for (i, scheduled, response) in &completed {
        let cursor = conn
            .query_row(
                "SELECT rowid FROM events WHERE event_id = ?1",
                [format!("open-loop-{i}")],
                |r| r.get::<_, i64>(0),
            )
            .unwrap() as u64;
        responses.push(response.duration_since(*scheduled).as_secs_f64() * 1000.0);
        let mut last = *scheduled;
        for id in 0..subscribers {
            let (_, observed, at) = observations
                .iter()
                .filter(|(client, observed, _)| *client == id && *observed >= cursor)
                .min_by_key(|(_, _, at)| *at)
                .expect("every subscriber catches every accepted revision or later");
            if *observed > cursor {
                coalesced += 1;
            }
            last = last.max(*at);
        }
        delivery.push(last.duration_since(*scheduled).as_secs_f64() * 1000.0);
    }
    println!(
        "subscriber_revision_coalesced={coalesced} subscriber_revision_total={}",
        completed.len() * subscribers
    );
    if !completed.is_empty() {
        report("scheduled_to_response", &mut responses);
        report("scheduled_to_all_subscribers", &mut delivery);
    }
    let service = service.lock().unwrap();
    for project in 0..projects {
        let run = if project == 0 {
            "benchmark-run".to_string()
        } else {
            format!("benchmark-run-{project}")
        };
        assert!(service.verify_chain(&run).unwrap().ok);
        let (actual, distinct): (i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), COUNT(DISTINCT event_id) FROM events WHERE run_id = ?1",
                [&run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let expected = count
            + completed
                .iter()
                .filter(|(i, _, _)| i % projects == project)
                .count();
        assert_eq!(actual as usize, expected);
        assert_eq!(distinct, actual);
    }
}

#[test]
#[ignore = "manual benchmark allocation accounting"]
fn index_logical_storage() {
    let categories = category::builtin().unwrap();
    let minimum = 1 + categories[0].milestones.len() * 2;
    let count = 100_000usize;
    let starts = (0..count - minimum).filter(|i| i % 3 == 1).count();
    let coordinate_size = std::mem::size_of::<(chrono::DateTime<Utc>, usize)>();
    let node_size = std::mem::size_of::<Option<chrono::DateTime<Utc>>>();
    println!(
        "count={count} starts={starts} coordinate_bytes={coordinate_size} tree_node_bytes={node_size} coordinate_and_tree_logical_bytes={}",
        starts * (coordinate_size + 4 * node_size)
    );
}
