//! Update checks in a host that installs updates itself, as the desktop app
//! does: a check that finds a newer release starts getting it ready, and
//! the check's status says how far that has got.

use std::sync::Arc;

use wayfetch_engine::protocol::update::{UpdateInstall, UpdateOutcome};
use wayfetch_engine::transfer::writer::BoxFuture;
use wayfetch_engine::updates::UpdateSource;

/// Gets a newer release ready to install, in the background.
pub trait Installer: std::fmt::Debug + Send + Sync + 'static {
    /// Starts getting `version` ready, unless it already is or is on its
    /// way. One that failed starts again.
    fn prepare(&self, version: &str);

    /// How far it has got, once [`Self::prepare`] was called.
    fn state(&self) -> Option<UpdateInstall>;
}

/// `source`'s checks, with `installer` getting each newer release ready.
pub(crate) struct Installing<S> {
    pub(crate) source: S,
    pub(crate) installer: Arc<dyn Installer>,
}

impl<S: UpdateSource> UpdateSource for Installing<S> {
    fn releases(&self) -> &str {
        self.source.releases()
    }

    fn check<'a>(&'a self, current: &'a str) -> BoxFuture<'a, UpdateOutcome> {
        Box::pin(async move {
            let outcome = self.source.check(current).await;
            if let UpdateOutcome::Available { version, .. } = &outcome {
                self.installer.prepare(version);
            }
            outcome
        })
    }

    fn install(&self) -> Option<UpdateInstall> {
        self.installer.state()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Newer;

    impl UpdateSource for Newer {
        fn releases(&self) -> &str {
            "https://example.test/releases"
        }

        fn check<'a>(&'a self, _: &'a str) -> BoxFuture<'a, UpdateOutcome> {
            Box::pin(async {
                UpdateOutcome::Available {
                    version: "0.2.0".into(),
                    url: "https://example.test/releases/v0.2.0".into(),
                }
            })
        }
    }

    #[derive(Debug, Default)]
    struct Recording(Mutex<Vec<String>>);

    impl Installer for Recording {
        fn prepare(&self, version: &str) {
            self.0.lock().unwrap().push(version.to_owned());
        }

        fn state(&self) -> Option<UpdateInstall> {
            Some(UpdateInstall::Ready)
        }
    }

    #[tokio::test]
    async fn a_newer_release_is_got_ready() {
        let installer = Arc::new(Recording::default());
        let source = Installing {
            source: Newer,
            installer: installer.clone(),
        };
        assert!(matches!(
            source.check("0.1.0").await,
            UpdateOutcome::Available { .. }
        ));
        assert_eq!(*installer.0.lock().unwrap(), ["0.2.0"]);
        assert_eq!(source.install(), Some(UpdateInstall::Ready));
        assert_eq!(source.releases(), "https://example.test/releases");
    }
}
