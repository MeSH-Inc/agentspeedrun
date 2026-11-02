//! `speedrun-commons`: the commons server. `SPEEDRUN_COMMONS_HOME` names its
//! data directory (database and key); `SPEEDRUN_COMMONS_ADDR` its listen
//! address, 127.0.0.1:7478 by default.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use speedrun_commons::{http, keys, store};

const DEFAULT_ADDR: &str = "127.0.0.1:7478";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let home = std::env::var_os("SPEEDRUN_COMMONS_HOME")
        .map(PathBuf::from)
        .context("SPEEDRUN_COMMONS_HOME must name the commons data directory")?;
    let addr = std::env::var("SPEEDRUN_COMMONS_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    let key = keys::load_or_create(&home.join("commons.key"))?;
    let store = store::Store::open(&home.join("commons.sqlite")).await?;
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, home = %home.display(), "commons listening");
    let app = http::router(http::AppState {
        store: Arc::new(store),
        key: Arc::new(key),
        verifier: Arc::new(speedrun_verify::Verifier::default()),
    });
    axum::serve(listener, app).await?;
    Ok(())
}
