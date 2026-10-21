//! Checking whether a newer release of the application is published,
//! through the host's [`UpdateSource`] and only when a client asks
//! (ADR-25). One check runs at a time and its answer is reused briefly, so
//! repeated asking sends one request.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wayfetch_protocol::update::{UpdateCheck, UpdateOutcome};
use wayfetch_protocol::{ErrorCode, ProtocolError, QueryResult};

use crate::transfer::writer::BoxFuture;

/// The application's version, which hosts and the engine share.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long an answer is reused.
const FRESH: Duration = Duration::from_secs(60);
/// How long a failed check is reused, so asking again soon tries again.
const FRESH_FAILURE: Duration = Duration::from_secs(15);
/// Most requests waiting for the check in progress.
const MAX_WAITING: usize = 4;

/// Where releases are published. The engine calls it only for
/// [`wayfetch_protocol::Query::Updates`].
pub trait UpdateSource: Send + Sync + 'static {
    /// Reads the newest published release and compares it with `current`.
    fn check<'a>(&'a self, current: &'a str) -> BoxFuture<'a, UpdateCheck>;
}

/// The host's source, if it gave one, and the last answer.
#[derive(Clone, Default)]
pub(crate) struct Updates(Option<Arc<Shared>>);

struct Shared {
    source: Arc<dyn UpdateSource>,
    last: Mutex<Option<(Instant, UpdateCheck)>>,
    waiting: Semaphore,
}

impl Updates {
    pub(crate) fn new(source: Option<Arc<dyn UpdateSource>>) -> Self {
        Self(source.map(|source| {
            Arc::new(Shared {
                source,
                last: Mutex::new(None),
                waiting: Semaphore::new(MAX_WAITING),
            })
        }))
    }

    /// Answers [`wayfetch_protocol::Query::Updates`], stopping at `cancel`.
    pub(crate) async fn check(
        self,
        cancel: CancellationToken,
    ) -> Result<QueryResult, ProtocolError> {
        let shared = self.0.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::NotApplicable,
                "this app does not check for updates",
            )
        })?;
        let _waiting = shared.waiting.try_acquire().map_err(|_| {
            ProtocolError::new(
                ErrorCode::Overloaded,
                "an update check is already in progress; try again shortly",
            )
        })?;
        let stopped = || ProtocolError::new(ErrorCode::ShuttingDown, "the engine is stopping");
        let mut last = cancel
            .run_until_cancelled(shared.last.lock())
            .await
            .ok_or_else(stopped)?;
        if let Some((at, check)) = last.as_ref() {
            let fresh = match check.outcome {
                UpdateOutcome::Unavailable { .. } => FRESH_FAILURE,
                _ => FRESH,
            };
            if at.elapsed() < fresh {
                return Ok(QueryResult::Updates(check.clone()));
            }
        }
        let check = cancel
            .run_until_cancelled(shared.source.check(VERSION))
            .await
            .ok_or_else(stopped)?;
        *last = Some((Instant::now(), check.clone()));
        Ok(QueryResult::Updates(check))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wayfetch_protocol::domain::name::DiagnosticText;

    use super::*;

    struct Counting(AtomicUsize);

    impl UpdateSource for Counting {
        fn check<'a>(&'a self, current: &'a str) -> BoxFuture<'a, UpdateCheck> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                UpdateCheck {
                    current: current.to_owned(),
                    releases: "https://example.test/releases".into(),
                    outcome: if n == 0 {
                        UpdateOutcome::Unavailable {
                            problem: DiagnosticText::new("offline"),
                        }
                    } else {
                        UpdateOutcome::UpToDate
                    },
                }
            })
        }
    }

    fn outcome(result: QueryResult) -> UpdateOutcome {
        match result {
            QueryResult::Updates(check) => {
                assert_eq!(check.current, VERSION);
                check.outcome
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn answers_are_reused_until_stale() {
        let source = Arc::new(Counting(AtomicUsize::new(0)));
        let updates = Updates::new(Some(source.clone()));
        let check = || updates.clone().check(CancellationToken::new());
        assert!(matches!(
            outcome(check().await.unwrap()),
            UpdateOutcome::Unavailable { .. }
        ));
        assert!(matches!(
            outcome(check().await.unwrap()),
            UpdateOutcome::Unavailable { .. }
        ));
        tokio::time::advance(FRESH_FAILURE).await;
        assert_eq!(outcome(check().await.unwrap()), UpdateOutcome::UpToDate);
        tokio::time::advance(FRESH - Duration::from_secs(1)).await;
        assert_eq!(outcome(check().await.unwrap()), UpdateOutcome::UpToDate);
        assert_eq!(source.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn without_a_source_nothing_is_checked() {
        let error = Updates::default()
            .check(CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotApplicable);
    }
}
