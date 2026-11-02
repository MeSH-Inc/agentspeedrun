//! Ordered daemon startup: the single-instance lock, the listener and the
//! data directory's client-visible names.
//!
//! The order lives here so a daemon that cannot take the lock or bind exits
//! before it writes a token, a database or an address file.

use std::fs;
use std::io::{ErrorKind, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use speedrun_core::paths;
use tokio::net::TcpListener;

/// Create the data directory and keep it private.
pub fn prepare_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", dir.display()))?;
    Ok(())
}

/// Bind the daemon's listener: `SPEEDRUN_ADDR`, else the default loopback
/// address, else a free loopback port when the default is taken. Returns the
/// listener and the address it bound.
pub async fn listen() -> Result<(TcpListener, SocketAddr)> {
    let listener = match paths::configured_addr() {
        Some(raw) => {
            let addr: SocketAddr = raw.parse().context("SPEEDRUN_ADDR must be host:port")?;
            require_loopback(addr)?;
            TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding {addr} (is another speedrund running?)"))?
        }
        None => match TcpListener::bind(paths::DEFAULT_ADDR).await {
            Ok(listener) => listener,
            Err(error) => {
                tracing::debug!(
                    %error,
                    "default port unavailable; choosing a free loopback port"
                );
                TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .context("binding a free loopback port")?
            }
        },
    };
    let addr = listener.local_addr().context("reading the bound address")?;
    Ok((listener, addr))
}

fn require_loopback(addr: SocketAddr) -> Result<()> {
    if !addr.ip().is_loopback() && std::env::var_os("SPEEDRUN_ALLOW_NON_LOOPBACK").is_none() {
        bail!("refusing to listen on non-loopback {addr}; set SPEEDRUN_ALLOW_NON_LOOPBACK=1 to override");
    }
    Ok(())
}

/// Read the token at `path`, creating it if it is absent. Creation writes a
/// temp file and hard-links it to the final name, so an existing token is
/// read and never replaced, even by a concurrent daemon.
pub fn ensure_token(path: &Path) -> Result<String> {
    if let Some(token) = read_token(path)? {
        return Ok(token);
    }
    create_token(path)
}

fn read_token(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim();
            if token.is_empty() {
                Ok(None)
            } else {
                Ok(Some(token.to_string()))
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn create_token(path: &Path) -> Result<String> {
    // Two ULIDs = 160 bits of randomness; good enough for a loopback secret.
    let token = format!("{}{}", speedrun_core::new_id(), speedrun_core::new_id());
    let temp = path.with_file_name(format!(".token-{}.tmp", speedrun_core::new_id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(token.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .with_context(|| format!("writing {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", temp.display()))?;
    drop(file);

    match fs::hard_link(&temp, path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            tracing::debug!(%error, "another daemon created the token first");
        }
        Err(error) => return Err(error).with_context(|| format!("linking {}", path.display())),
    }
    fs::remove_file(&temp).with_context(|| format!("removing {}", temp.display()))?;
    read_token(path)?.ok_or_else(|| {
        anyhow!(
            "token file {} is empty; delete it and restart speedrund to create a new one",
            path.display()
        )
    })
}

/// Write the bound `host:port` to `$DATA_DIR/addr` (0600) by renaming a temp
/// file over it, so a reader never sees a partial address.
pub fn write_addr(dir: &Path, addr: SocketAddr) -> Result<()> {
    let path = dir.join("addr");
    let temp = dir.join(format!(".addr-{}.tmp", speedrun_core::new_id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(addr.to_string().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .with_context(|| format!("writing {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", temp.display()))?;
    drop(file);
    fs::rename(&temp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
