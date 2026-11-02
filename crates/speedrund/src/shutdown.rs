//! The daemon's one shutdown decision: which signals stop it, how open streams
//! learn about it, and how long connections may take to drain.
//!
//! Callers never see the signal plumbing or the deadline: `main` hands the
//! server to `Shutdown::run`, and the HTTP layer reads a single watch flag.

use std::future::IntoFuture;
use std::time::Duration;

use anyhow::Result;
use tokio::signal::unix::SignalKind;
use tokio::sync::watch;
use tokio::time::sleep;

/// How long connections may take to drain once shutdown has started. Open
/// live streams end on the same signal, so this bounds only stragglers.
const GRACE: Duration = Duration::from_secs(2);

/// Owns the stop signal and the drain deadline. Cheap to clone.
#[derive(Clone)]
pub struct Shutdown {
    started: watch::Sender<bool>,
}

impl Shutdown {
    /// Install the SIGINT/SIGTERM handler. A failure here happens before the
    /// daemon has any side effect, so startup can abort cleanly.
    pub fn install() -> Result<Self> {
        let (started, _keepalive) = watch::channel(false);
        let shutdown = Self { started };
        let signal = shutdown.clone();
        let mut term = tokio::signal::unix::signal(SignalKind::terminate())?;
        let _signal = tokio::spawn(async move {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(e) = result {
                        tracing::warn!("listening for ctrl-c failed: {e}");
                    }
                }
                _ = term.recv() => {}
            }
            signal.request();
        });
        Ok(shutdown)
    }

    /// Ask the daemon to stop, exactly as a signal does.
    ///
    /// This is the only stop a non-signal caller gets: it sets the same flag the
    /// signal handlers set, so live streams end, the server drains, and `run`
    /// applies the same deadline. Callers never see the channel that carries it.
    pub fn request(&self) {
        tracing::info!("shutting down");
        self.started.send_replace(true);
    }

    /// A receiver that reports when shutdown has started. Live streams select
    /// on it and end, so they cannot hold the process open.
    pub fn watch(&self) -> watch::Receiver<bool> {
        self.started.subscribe()
    }

    /// Resolve once shutdown has started.
    pub async fn wait(self) {
        let mut started = self.started.subscribe();
        loop {
            if *started.borrow_and_update() {
                return;
            }
            if started.changed().await.is_err() {
                return;
            }
        }
    }

    /// Run the server until a signal stops it, then stop waiting for
    /// connections after `GRACE`.
    pub async fn run<F>(self, server: F) -> Result<()>
    where
        F: IntoFuture<Output = std::io::Result<()>>,
    {
        let deadline = self.clone();
        let server = server.into_future();
        tokio::pin!(server);
        tokio::select! {
            result = &mut server => Ok(result?),
            () = deadline.after_signal() => {
                tracing::warn!(
                    "graceful shutdown did not finish within {}s; exiting",
                    GRACE.as_secs()
                );
                Ok(())
            }
        }
    }

    /// Resolve when shutdown starts, then wait for the drain deadline.
    async fn after_signal(self) {
        self.wait().await;
        sleep(GRACE).await;
    }
}
