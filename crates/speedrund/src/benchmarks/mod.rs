//! Opt-in local performance measurements; never part of the normal test run.
mod history;

use crate::{
    health::Health,
    http::{self, AppState},
    service::Service,
    shutdown::Shutdown,
    store::Store,
};
use chrono::{Duration, Utc};
use speedrun_core::{EventPayload, RunView, api::AppendRequest, category};
use std::{
    io::{BufRead, BufReader as StdBufReader},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, watch},
};

fn report(name: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100).saturating_sub(1)];
    println!(
        "{name} n={} p50_ms={:.3} p95_ms={:.3}",
        samples.len(),
        percentile(50),
        percentile(95)
    );
}

async fn request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer benchmark\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    response
}

fn subscribe(addr: std::net::SocketAddr, id: usize, tx: mpsc::UnboundedSender<(usize, u64, Instant)>) {
    // ureq decodes HTTP transfer framing before exposing the SSE body.
    let response = ureq::get(&format!("http://{addr}/v1/live/stream"))
        .set("Authorization", "Bearer benchmark")
        .call()
        .unwrap();
    let mut event_id = None;
    let mut data = String::new();
    for line in StdBufReader::new(response.into_reader()).lines() {
        let Ok(line) = line else { return };
        if let Some(value) = line.strip_prefix("id: ") {
            event_id = value.parse::<u64>().ok();
        } else if let Some(value) = line.strip_prefix("data: ") {
            data.push_str(value);
        } else if line.is_empty() {
            if let Some(cursor) = event_id.take() {
                let snapshot: speedrun_core::live::LiveSnapshot = serde_json::from_str(&data).unwrap();
                assert_eq!(snapshot.cursor, cursor);
                if tx.send((id, cursor, Instant::now())).is_err() {
                    return;
                }
            }
            data.clear();
        }
    }
}

async fn wait_cursor(
    rx: &mut mpsc::UnboundedReceiver<(usize, u64, Instant)>,
    clients: usize,
    cursor: u64,
) -> Instant {
    let mut seen = vec![false; clients];
    let mut last = None::<Instant>;
    while seen.iter().any(|v| !v) {
        let (id, observed, received) = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
            .await
            .expect("subscriber timeout")
            .expect("subscriber disconnected");
        if observed >= cursor {
            seen[id] = true;
            last = Some(last.map_or(received, |t| t.max(received)));
        }
    }
    last.expect("at least one subscriber")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual performance benchmark"]
async fn history_pressure() {
    let count: usize = std::env::var("BENCH_EVENTS")
        .unwrap_or_else(|_| "1000".into())
        .parse()
        .unwrap();
    let subscribers: usize = std::env::var("BENCH_SUBSCRIBERS")
        .unwrap_or_else(|_| "1".into())
        .parse()
        .unwrap();
    let iterations: usize = std::env::var("BENCH_ITERATIONS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .unwrap();
    assert!(subscribers > 0 && iterations > 0);
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(
        &dir.path().join("speedrun.sqlite"),
        &dir.path().join("device.key"),
    )
    .unwrap();
    let started = Instant::now();
    history::seed(&mut store, count).unwrap();
    println!(
        "seed events={count} subscribers={subscribers} seconds={:.2}",
        started.elapsed().as_secs_f64()
    );
    let mut load = Vec::new();
    let mut folds = Vec::new();
    for _ in 0..iterations {
        let start = Instant::now();
        let events = store.reader().run_events("benchmark-run").unwrap();
        load.push(start.elapsed().as_secs_f64() * 1000.0);
        let start = Instant::now();
        std::hint::black_box(RunView::fold(&events, Utc::now()).unwrap());
        folds.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report("load_deserialize", &mut load);
    report("fold", &mut folds);
    let service = Service::new(store, category::builtin().unwrap(), "benchmark".into());
    assert!(service.verify_chain("benchmark-run").unwrap().ok);
    let state = AppState {
        service: Arc::new(Mutex::new(service)),
        token: Arc::new("benchmark".into()),
        display_token: Arc::new("benchmark".into()),
        agent_token: Arc::new("benchmark-agent".into()),
        health: Arc::new(Health::resolve(dir.path()).unwrap()),
        verifier: Arc::new(speedrun_verify::Verifier::default()),
        changes: Arc::new(watch::channel(0).0),
        shutdown: Shutdown::install().unwrap(),
        live_hub: Arc::default(),
    };
    // Warm the personal-best cache before recording steady-state HTTP calls.
    state.service.lock().unwrap().live(Duration::hours(24)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = state.shutdown.clone();
    let server = tokio::spawn(async move { axum::serve(listener, http::router(state, &[])).await.unwrap() });
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for id in 0..subscribers {
        tasks.push(tokio::task::spawn_blocking({
            let tx = tx.clone();
            move || subscribe(addr, id, tx)
        }));
    }
    wait_cursor(&mut rx, subscribers, count as u64).await;
    let mut gets = Vec::new();
    let mut writes = Vec::new();
    let mut delivery = Vec::new();
    // Continue a realistic metadata-only prompt and paired agent-turn cycle.
    let workload_start = Instant::now();
    for i in 0..iterations {
        let start = Instant::now();
        request(addr, "GET", "/v1/live", "").await;
        gets.push(start.elapsed().as_secs_f64() * 1000.0);
        let session_id = format!("measured-session-{}", (i / 3) % 8);
        let payload = match i % 3 {
            0 => EventPayload::PromptSubmitted {
                session_id,
                host: "benchmark".into(),
                model: Some("example-model".into()),
                content_digest: String::new(),
                length: 80,
                redacted: false,
            },
            1 => EventPayload::AgentTurnStarted { session_id },
            _ => EventPayload::AgentTurnEnded { session_id },
        };
        let body = serde_json::to_string(&AppendRequest {
            event_id: format!("measured-{i}"),
            run_id: "benchmark-run".into(),
            occurred_at: None,
            payload,
        })
        .unwrap();
        let start = Instant::now();
        request(addr, "POST", "/v1/events", &body).await;
        writes.push(start.elapsed().as_secs_f64() * 1000.0);
        let received = wait_cursor(&mut rx, subscribers, (count + i + 1) as u64).await;
        delivery.push(received.duration_since(start).as_secs_f64() * 1000.0);
    }
    println!(
        "closed_loop_cycles_per_second={:.3}",
        iterations as f64 / workload_start.elapsed().as_secs_f64()
    );
    report("http_get_live", &mut gets);
    report("http_append_response", &mut writes);
    report("append_to_all_subscribers", &mut delivery);
    shutdown.request();
    for task in tasks {
        task.await.unwrap();
    }
    server.abort();
}
