//! The HTTP/1.1 listener: bounded connections, a deadline for request
//! headers, and a graceful drain.
//!
//! Connections are served with hyper directly rather than `axum::serve`,
//! which offers neither a connection limit nor a header deadline. At the
//! limit, the listener stops accepting and further clients wait in the
//! kernel's bounded backlog instead of costing memory here.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::ApiLimits;

/// Pause after a failed accept, such as running out of file descriptors,
/// so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Serves `router` on `listener` until `shutdown` is cancelled, then stops
/// accepting, asks open connections to close after their current request,
/// and waits up to `limits.drain_timeout` for them. Returns whether every
/// connection closed in time.
pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    limits: &ApiLimits,
    shutdown: CancellationToken,
) -> bool {
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    let connections = Arc::new(Semaphore::new(limits.max_connections.get()));
    let graceful = GracefulShutdown::new();
    loop {
        let permit = tokio::select! {
            () = shutdown.cancelled() => break,
            permit = connections.clone().acquire_owned() => {
                permit.expect("the semaphore is never closed")
            }
        };
        let stream = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => {
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
        };
        let service = TowerToHyperService::new(router.clone());
        let connection = graceful.watch(builder.serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            // A broken connection concerns only its client.
            let _ = connection.await;
            drop(permit);
        });
    }
    drop(listener);
    tokio::time::timeout(limits.drain_timeout, graceful.shutdown())
        .await
        .is_ok()
}
