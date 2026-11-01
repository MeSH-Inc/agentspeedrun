//! speedrund: the local agentspeedrun daemon. Owns the event log; every
//! editor and agent plugin talks to it over loopback HTTP.

mod http;
mod keys;
mod logging;
mod service;
mod store;
mod verify;

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use speedrun_core::{category, paths, sign::pubkey_hex};

fn ensure_token(path: &Path) -> Result<String> {
    if let Ok(t) = fs::read_to_string(path) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    // Two ULIDs = 160 bits of randomness; good enough for a loopback secret.
    let token = format!("{}{}", speedrun_core::new_id(), speedrun_core::new_id());
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(token.as_bytes())?;
    Ok(token)
}

#[tokio::main]
async fn main() -> Result<()> {
    logging::init()?;

    let dir = paths::data_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let token = ensure_token(&paths::token_path())?;
    let display_token = ensure_token(&paths::display_token_path())?;
    let device_key = keys::load_or_create(&paths::device_key_path())?;
    let device_pubkey = pubkey_hex(&device_key);

    let store = store::Store::open(&dir.join("speedrun.sqlite"), device_key)?;
    let service = service::Service::new(store, category::builtin());

    let addr: SocketAddr = paths::daemon_addr()
        .parse()
        .context("SPEEDRUN_ADDR must be host:port")?;
    if !addr.ip().is_loopback() && std::env::var_os("SPEEDRUN_ALLOW_NON_LOOPBACK").is_none() {
        bail!("refusing to listen on non-loopback {addr}; set SPEEDRUN_ALLOW_NON_LOOPBACK=1 to override");
    }
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} (is another speedrund running?)"))?;
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
        },
        &cors_origins,
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
