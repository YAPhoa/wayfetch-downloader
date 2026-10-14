//! Process signals that ask a host to stop.

/// Requests to stop: Ctrl+C, and on Unix also `SIGTERM` from a service
/// manager.
pub struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl Signals {
    /// Starts listening. Register before starting the engine, so a signal
    /// during startup still leads to a graceful shutdown.
    #[cfg(unix)]
    pub fn new() -> Result<Self, String> {
        use tokio::signal::unix::{SignalKind, signal};
        let listen = |kind| signal(kind).map_err(|e| format!("cannot handle signals: {e}"));
        Ok(Self {
            interrupt: listen(SignalKind::interrupt())?,
            terminate: listen(SignalKind::terminate())?,
        })
    }

    /// Starts listening.
    #[cfg(not(unix))]
    pub fn new() -> Result<Self, String> {
        Ok(Self {})
    }

    /// Completes at the next request to stop.
    #[cfg(unix)]
    pub async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }

    /// Completes at the next request to stop.
    #[cfg(not(unix))]
    pub async fn recv(&mut self) {
        let _ = tokio::signal::ctrl_c().await;
    }
}
