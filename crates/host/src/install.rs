//! Update checks in a host that installs updates itself, as the desktop app
//! does: the check's status says how far installing the newer release has
//! got, which the person starts.

use std::sync::Arc;

use wayfetch_engine::protocol::update::{UpdateInstall, UpdateOutcome};
use wayfetch_engine::transfer::writer::BoxFuture;
use wayfetch_engine::updates::UpdateSource;

/// Installs a newer release when the person asks.
pub trait Installer: std::fmt::Debug + Send + Sync + 'static {
    /// How far installing the newer release has got.
    fn state(&self) -> UpdateInstall;
}

/// `source`'s checks, with `installer`'s progress.
pub(crate) struct Installing<S> {
    pub(crate) source: S,
    pub(crate) installer: Arc<dyn Installer>,
}

impl<S: UpdateSource> UpdateSource for Installing<S> {
    fn releases(&self) -> &str {
        self.source.releases()
    }

    fn check<'a>(&'a self, current: &'a str) -> BoxFuture<'a, UpdateOutcome> {
        self.source.check(current)
    }

    fn install(&self) -> Option<UpdateInstall> {
        Some(self.installer.state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Newer;

    impl UpdateSource for Newer {
        fn releases(&self) -> &str {
            "https://example.test/releases"
        }

        fn check<'a>(&'a self, _: &'a str) -> BoxFuture<'a, UpdateOutcome> {
            Box::pin(async { UpdateOutcome::UpToDate })
        }
    }

    #[derive(Debug)]
    struct Waiting;

    impl Installer for Waiting {
        fn state(&self) -> UpdateInstall {
            UpdateInstall::Idle
        }
    }

    #[tokio::test]
    async fn checks_pass_through_and_installing_is_reported() {
        let source = Installing {
            source: Newer,
            installer: Arc::new(Waiting),
        };
        assert_eq!(source.check("0.1.0").await, UpdateOutcome::UpToDate);
        assert_eq!(source.install(), Some(UpdateInstall::Idle));
        assert_eq!(source.releases(), "https://example.test/releases");
    }
}
