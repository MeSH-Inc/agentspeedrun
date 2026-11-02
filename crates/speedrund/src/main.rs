//! speedrund: the local agentspeedrun daemon. Owns the event log; every
//! editor and agent plugin talks to it over loopback HTTP.

mod clock;
mod datadir_lock;
mod disclosure;
mod git;
mod health;
mod http;
mod keys;
mod logging;
mod provenance;
mod service;
mod shutdown;
mod startup;
mod store;
mod verify;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use speedrun_core::{category, paths};

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    // Install the stop signal before any startup side effect, so a failure here
    // leaves the data directory untouched.
    let shutdown = shutdown::Shutdown::install()?;

    let dir = paths::data_dir();
    startup::prepare_dir(&dir)?;
    // One daemon owns a data directory for its whole life; a second one exits
    // before it can write anything, and the operating system releases the lock
    // however the first one ends.
    let _lock = datadir_lock::DataDirLock::acquire(&dir)?;
    let (listener, addr) = startup::listen().await?;
    let token = startup::ensure_token(&paths::token_path())?;
    let display_token = startup::ensure_token(&paths::display_token_path())?;
    // Agent tools use their own token, created the same way: a second file a
    // process can read, never overwrite.
    let agent_token = startup::ensure_token(&paths::agent_token_path())?;
    let health = health::Health::resolve(&dir)?;
    let store = store::Store::open(&dir.join("speedrun.sqlite"), &paths::device_key_path())?;
    let device_pubkey = store.device_pubkey();
    startup::write_addr(&dir, addr)?;
    let mut service = service::Service::new(store, category::builtin());
    // A snapshot names this daemon by the data_dir_id `/v1/health` reports;
    // `Health` owns that value, so it is passed in rather than recomputed.
    service.serve_as(health.data_dir_id().to_string());
    logging::startup(addr, &dir, &device_pubkey);

    let cors_origins: Vec<String> = std::env::var("SPEEDRUN_CORS_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(str::to_string)
        .collect();
    let app = http::router(
        http::AppState {
            service: Arc::new(Mutex::new(service)),
            token: Arc::new(token),
            display_token: Arc::new(display_token),
            agent_token: Arc::new(agent_token),
            health: Arc::new(health),
            verifier: Arc::new(verify::Verifier::default()),
            changes: Arc::new(tokio::sync::watch::channel(0).0),
            shutdown: shutdown.clone(),
        },
        &cors_origins,
    );
    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().wait());
    shutdown.run(serve).await
}
