//! speedrund: the local agentspeedrun daemon. Owns the event log; every
//! editor and agent plugin talks to it over loopback HTTP.

mod clock;
mod disclosure;
mod git;
mod http;
mod keys;
mod logging;
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
    // The lock is held for the whole process life, so a second daemon on this
    // data directory exits before it can write anything. The operating system
    // releases it however the process ends.
    let _lock = startup::DataDirLock::acquire(&dir)?;
    let (listener, addr) = startup::listen().await?;
    let token = startup::ensure_token(&paths::token_path())?;
    let display_token = startup::ensure_token(&paths::display_token_path())?;
    let store = store::Store::open(&dir.join("speedrun.sqlite"), &paths::device_key_path())?;
    let device_pubkey = store.device_pubkey();
    startup::write_addr(&dir, addr)?;
    let service = service::Service::new(store, category::builtin());
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
            verifier: Arc::new(verify::Verifier::default()),
            changes: Arc::new(tokio::sync::watch::channel(0).0),
            shutdown: shutdown.watch(),
        },
        &cors_origins,
    );
    let serve = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().wait());
    shutdown.run(serve).await
}
