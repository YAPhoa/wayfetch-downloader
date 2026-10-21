//! Checking whether a newer release of the application is published,
//! through the host's [`UpdateSource`] (ADR-25): when a client asks, and
//! while automatic updates are on, shortly after start and then daily. One
//! check runs at a time and an answer is reused briefly, so repeated asking
//! sends one request.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use wayfetch_protocol::domain::time::UtcTimestamp;
use wayfetch_protocol::update::{UpdateCheck, UpdateInstall, UpdateOutcome};
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
/// How long after start the first automatic check waits, so a short run
/// sends nothing.
const FIRST_CHECK: Duration = Duration::from_secs(60);
/// How often automatic checks run after that.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Where releases are published, and, in a host that installs updates
/// itself, its installation of the newer one.
pub trait UpdateSource: Send + Sync + 'static {
    /// Where every release is listed.
    fn releases(&self) -> &str;

    /// Reads the newest published release and compares it with `current`.
    fn check<'a>(&'a self, current: &'a str) -> BoxFuture<'a, UpdateOutcome>;

    /// How far installing the newer release has got, if the host installs
    /// updates.
    fn install(&self) -> Option<UpdateInstall> {
        None
    }
}

/// The host's source, if it gave one, the last answer, and whether checks
/// run on their own.
#[derive(Clone)]
pub(crate) struct Updates {
    shared: Option<Arc<Shared>>,
    automatic: Arc<watch::Sender<bool>>,
}

struct Shared {
    source: Arc<dyn UpdateSource>,
    /// The last check and when it ended.
    last: Mutex<Option<(Instant, UtcTimestamp, UpdateOutcome)>>,
    /// Held while a check runs.
    checking: tokio::sync::Mutex<()>,
    waiting: Semaphore,
}

impl Updates {
    pub(crate) fn new(source: Option<Arc<dyn UpdateSource>>, automatic: bool) -> Self {
        Self {
            shared: source.map(|source| {
                Arc::new(Shared {
                    source,
                    last: Mutex::new(None),
                    checking: tokio::sync::Mutex::new(()),
                    waiting: Semaphore::new(MAX_WAITING),
                })
            }),
            automatic: Arc::new(watch::Sender::new(automatic)),
        }
    }

    /// Whether checks run on their own.
    pub(crate) fn automatic(&self) -> bool {
        *self.automatic.borrow()
    }

    /// Turns automatic checks on, which checks at once, or off.
    pub(crate) fn set_automatic(&self, on: bool) {
        self.automatic.send_if_modified(|current| {
            let changed = *current != on;
            *current = on;
            changed
        });
    }

    /// Answers [`wayfetch_protocol::Query::Updates`]: the last check, or,
    /// with `check`, a new one, stopping at `cancel`.
    pub(crate) async fn answer(
        self,
        check: bool,
        cancel: CancellationToken,
    ) -> Result<QueryResult, ProtocolError> {
        let shared = self.shared.ok_or_else(|| {
            ProtocolError::new(
                ErrorCode::NotApplicable,
                "this app does not check for updates",
            )
        })?;
        if check {
            let _waiting = shared.waiting.try_acquire().map_err(|_| {
                ProtocolError::new(
                    ErrorCode::Overloaded,
                    "an update check is already in progress; try again shortly",
                )
            })?;
            cancel
                .run_until_cancelled(shared.refresh(FRESH))
                .await
                .ok_or_else(|| {
                    ProtocolError::new(ErrorCode::ShuttingDown, "the engine is stopping")
                })?;
        }
        Ok(QueryResult::Updates(shared.status()))
    }

    /// Checks shortly after start and then daily while automatic checks
    /// are on, until `cancel`.
    pub(crate) async fn run(self, cancel: CancellationToken) {
        let Some(shared) = self.shared else {
            return;
        };
        let mut automatic = self.automatic.subscribe();
        let mut wait = FIRST_CHECK;
        let _ = cancel
            .run_until_cancelled(async {
                loop {
                    if *automatic.borrow_and_update() {
                        tokio::select! {
                            () = tokio::time::sleep(wait) => {
                                shared.refresh(FRESH).await;
                                wait = EVERY;
                            }
                            _ = automatic.changed() => {}
                        }
                    } else {
                        // Turned on again: check now.
                        if automatic.changed().await.is_err() {
                            return;
                        }
                        wait = Duration::ZERO;
                    }
                }
            })
            .await;
    }
}

impl Shared {
    /// Checks unless the last check is younger than `fresh` (or a failure
    /// younger than [`FRESH_FAILURE`]), waiting for one in progress.
    async fn refresh(&self, fresh: Duration) {
        let _checking = self.checking.lock().await;
        if let Some((at, _, outcome)) = self.last.lock().unwrap().as_ref() {
            let fresh = match outcome {
                UpdateOutcome::Unavailable { .. } => fresh.min(FRESH_FAILURE),
                _ => fresh,
            };
            if at.elapsed() < fresh {
                return;
            }
        }
        let outcome = self.source.check(VERSION).await;
        let now = UtcTimestamp::from_system_time(SystemTime::now());
        *self.last.lock().unwrap() = Some((Instant::now(), now, outcome));
    }

    /// The last check, with the host's installation of its release.
    fn status(&self) -> UpdateCheck {
        let last = self.last.lock().unwrap().clone();
        let (checked_at, outcome) = match last {
            Some((_, at, outcome)) => (Some(at), outcome),
            None => (None, UpdateOutcome::NotChecked),
        };
        let install = match outcome {
            UpdateOutcome::Available { .. } => self.source.install(),
            _ => None,
        };
        UpdateCheck {
            current: VERSION.to_owned(),
            releases: self.source.releases().to_owned(),
            outcome,
            checked_at,
            install,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wayfetch_protocol::domain::name::DiagnosticText;

    use super::*;

    /// Fails its first check, then finds nothing newer.
    struct Counting(AtomicUsize);

    impl UpdateSource for Counting {
        fn releases(&self) -> &str {
            "https://example.test/releases"
        }

        fn check<'a>(&'a self, _: &'a str) -> BoxFuture<'a, UpdateOutcome> {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 0 {
                    UpdateOutcome::Unavailable {
                        problem: DiagnosticText::new("offline"),
                    }
                } else {
                    UpdateOutcome::UpToDate
                }
            })
        }
    }

    fn outcome(result: QueryResult) -> UpdateOutcome {
        match result {
            QueryResult::Updates(check) => {
                assert_eq!(check.current, VERSION);
                assert_eq!(check.releases, "https://example.test/releases");
                assert_eq!(
                    check.checked_at.is_some(),
                    check.outcome != UpdateOutcome::NotChecked
                );
                check.outcome
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn answers_are_reused_until_stale() {
        let source = Arc::new(Counting(AtomicUsize::new(0)));
        let updates = Updates::new(Some(source.clone()), false);
        let ask = |check| updates.clone().answer(check, CancellationToken::new());
        assert_eq!(
            outcome(ask(false).await.unwrap()),
            UpdateOutcome::NotChecked
        );
        assert!(matches!(
            outcome(ask(true).await.unwrap()),
            UpdateOutcome::Unavailable { .. }
        ));
        assert!(matches!(
            outcome(ask(true).await.unwrap()),
            UpdateOutcome::Unavailable { .. }
        ));
        tokio::time::advance(FRESH_FAILURE).await;
        assert_eq!(outcome(ask(true).await.unwrap()), UpdateOutcome::UpToDate);
        tokio::time::advance(FRESH - Duration::from_secs(1)).await;
        assert_eq!(outcome(ask(true).await.unwrap()), UpdateOutcome::UpToDate);
        // Reading the status never checks.
        assert_eq!(outcome(ask(false).await.unwrap()), UpdateOutcome::UpToDate);
        assert_eq!(source.0.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn automatic_checks_wait_after_start_then_run_daily() {
        let source = Arc::new(Counting(AtomicUsize::new(0)));
        let updates = Updates::new(Some(source.clone()), true);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(updates.clone().run(cancel.clone()));
        let checks = || source.0.load(Ordering::SeqCst);
        tokio::time::sleep(FIRST_CHECK - Duration::from_secs(1)).await;
        assert_eq!(checks(), 0);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(checks(), 1);
        tokio::time::sleep(EVERY).await;
        assert_eq!(checks(), 2);
        // Off: nothing more; on again: at once.
        updates.set_automatic(false);
        tokio::time::sleep(EVERY * 2).await;
        assert_eq!(checks(), 2);
        updates.set_automatic(true);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(checks(), 3);
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn without_a_source_nothing_is_checked() {
        let error = Updates::new(None, true)
            .answer(true, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::NotApplicable);
    }
}
