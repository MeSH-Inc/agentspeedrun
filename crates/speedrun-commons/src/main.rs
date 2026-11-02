//! `speedrun-commons`: the commons server. `SPEEDRUN_COMMONS_HOME` names its
//! data directory (database and key); `SPEEDRUN_COMMONS_ADDR` its listen
//! address, else `0.0.0.0:$PORT` when `PORT` is set, else 127.0.0.1:7478.
//! `speedrun-commons backup <file>` copies the database for safekeeping.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use speedrun_commons::{github, http, judge, keys, store};

const DEFAULT_ADDR: &str = "127.0.0.1:7478";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let home = commons_home()?;
    let mut args = std::env::args_os().skip(1);
    let command = args.next();
    let file = args.next();
    match command.as_deref() {
        Some(command) if command == OsStr::new("backup") => {
            let file = file.context("backup needs a file path")?;
            backup(&home, Path::new(&file)).await
        }
        _ => serve(home).await,
    }
}

/// The commons data directory, from `SPEEDRUN_COMMONS_HOME`.
fn commons_home() -> Result<PathBuf> {
    std::env::var_os("SPEEDRUN_COMMONS_HOME")
        .map(PathBuf::from)
        .context("SPEEDRUN_COMMONS_HOME must name the commons data directory")
}

/// Where to listen: `SPEEDRUN_COMMONS_ADDR` when set, else `PORT` on all
/// interfaces, else the loopback default.
fn listen_addr() -> Result<String> {
    match std::env::var("SPEEDRUN_COMMONS_ADDR") {
        Ok(addr) => Ok(addr),
        Err(std::env::VarError::NotPresent) => match std::env::var("PORT") {
            Ok(port) => match port.trim().parse::<u16>() {
                Ok(port) => Ok(format!("0.0.0.0:{port}")),
                Err(error) => Err(anyhow::anyhow!("PORT {port:?} is not a port number: {error}")),
            },
            Err(std::env::VarError::NotPresent) => Ok(DEFAULT_ADDR.to_string()),
            Err(error) => Err(anyhow::anyhow!("PORT is not text: {error}")),
        },
        Err(error) => Err(anyhow::anyhow!("SPEEDRUN_COMMONS_ADDR is not text: {error}")),
    }
}

/// Serve the commons until it is stopped.
async fn serve(home: PathBuf) -> Result<()> {
    let addr = listen_addr()?;
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
        verifier: Arc::new(judge::verifier()),
        publishing: Arc::new(tokio::sync::Mutex::new(())),
        github: Arc::new(github::GitHubConfig::from_env()),
        public_url: Arc::new(public_url()),
    });
    axum::serve(listener, app).await?;
    Ok(())
}

/// `backup <file>`: copy the database with `VACUUM INTO`, which is consistent
/// while a server is writing, and refuse to overwrite an existing file.
async fn backup(home: &Path, file: &Path) -> Result<()> {
    if file
        .try_exists()
        .with_context(|| format!("checking {}", file.display()))?
    {
        return Err(anyhow::anyhow!("{} already exists", file.display()));
    }
    let database_path = home.join("commons.sqlite");
    let database_exists = database_path
        .try_exists()
        .with_context(|| format!("checking {}", database_path.display()))?;
    if !database_exists {
        return Err(anyhow::anyhow!("{} does not exist", database_path.display()));
    }
    let target = file.to_str().context("the backup path is not valid UTF-8")?;
    let database = store::Store::open(&database_path).await?;
    sqlx::query("VACUUM INTO ?")
        .bind(target)
        .execute(&database.pool)
        .await
        .with_context(|| format!("backing up into {}", file.display()))?;
    // Count the copy, not the source: a publication may land in between.
    let copy = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(file)
                .read_only(true),
        )
        .await
        .with_context(|| format!("opening the backup {}", file.display()))?;
    let leaves: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM log_leaves")
        .fetch_one(&copy)
        .await
        .with_context(|| format!("counting the leaves in {}", file.display()))?;
    copy.close().await;
    let mut out = std::io::stdout().lock();
    writeln!(out, "backed up {leaves} leaves to {}", file.display()).context("writing the backup receipt")?;
    Ok(())
}

/// `SPEEDRUN_COMMONS_PUBLIC_URL` without a trailing slash, else `$COMMONS`.
fn public_url() -> String {
    match std::env::var("SPEEDRUN_COMMONS_PUBLIC_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_string(),
        _ => "$COMMONS".to_string(),
    }
}
