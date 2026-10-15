//! Where downloads may be saved: the destination roots a client may
//! choose, and the default for new jobs (architecture brief sections 4.7
//! and 8.6, ADR-17 and ADR-29).
//!
//! * A root is permitted while the operator configures its path, or after
//!   a host registered it on its user's explicit request, such as a folder
//!   chosen in the desktop app's native picker. Nothing a client sends can
//!   add one: clients choose among permitted roots by ID.
//! * Each root's ID is allocated by the store and written into its
//!   sentinel ([`initialize_root`]). A configured path the store has not
//!   seen is identified from its sentinel, so a root earlier versions
//!   marked keeps its ID; one without a sentinel waits until the operator
//!   initializes it, and is retried as work arrives.
//! * Each job records its root when it is submitted, so changing the
//!   default never moves a job. A job submitted while the operator's
//!   download root is configured but not identified yet (an unmounted
//!   drive) belongs to that root and waits for it, never taking a default
//!   chosen later. A job whose root is no longer permitted is blocked
//!   rather than redirected.
//! * Identifying, probing, and registering roots never run on the
//!   coordinator or while the store is held, since a hung mount would
//!   stall every request. Checks
//!   run on blocking threads with a deadline, at most one per path at a
//!   time ([`Probes`]), so a hung mount costs one blocked thread and is
//!   reported as not responding. Registering a folder runs as its own
//!   task ([`register`]) that uses the store only between its filesystem
//!   steps.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use wayfetch_domain::id::RootId;
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;
use wayfetch_protocol::query::{RootSummary, Settings};
use wayfetch_protocol::{ErrorCode, InstanceId, ProtocolError};

use crate::config::EngineConfig;
use crate::fs::{FsError, Root};
use crate::lifecycle::StoreClient;
use crate::state_dir::StateDir;
use crate::store::{Store, StoreError, path_text};

/// How long an availability check may take before the root is reported as
/// not responding.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A permitted root and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Destination {
    pub(crate) id: RootId,
    pub(crate) path: PathBuf,
}

/// Why a root cannot be initialized or registered.
#[derive(Debug, thiserror::Error)]
pub enum RootError {
    /// The path is relative.
    #[error("{} is not an absolute path", .0.display())]
    Relative(PathBuf),
    /// No more roots can be permitted.
    #[error("at most {} destinations are supported", EngineConfig::MAX_ROOTS)]
    TooMany,
    /// The directory is unavailable or marked for another instance.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// The store refused, for example because the folder's sentinel names a
    /// root recorded at another path.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<RootError> for ProtocolError {
    fn from(err: RootError) -> Self {
        match err {
            RootError::Relative(_) => {
                ProtocolError::new(ErrorCode::InvalidRequest, &err.to_string())
            }
            RootError::TooMany => ProtocolError::new(ErrorCode::LimitExceeded, &err.to_string()),
            RootError::Fs(_) => ProtocolError::new(
                ErrorCode::NotApplicable,
                &format!("the folder cannot be used: {err}"),
            ),
            RootError::Store(e) => e.into(),
        }
    }
}

fn now() -> UtcTimestamp {
    UtcTimestamp::from_system_time(SystemTime::now())
}

/// Marks the existing directory `path` as a destination root of the
/// instance in `state_dir`, giving it a new ID unless it already has one.
/// Idempotent. This is the operator's explicit action (`init-root`); the
/// engine never initializes a configured root by itself. Blocking; the
/// caller holds the state directory, so no engine is running on it.
pub fn initialize_root(state_dir: &StateDir, path: &Path) -> Result<RootId, RootError> {
    let mut store = Store::open(state_dir.path())?;
    initialize(&mut store, path, state_dir.identity().instance, false)
}

/// Refuses a path the store could not record.
fn check_path(path: &Path) -> Result<(), RootError> {
    if !path.is_absolute() {
        return Err(RootError::Relative(path.to_owned()));
    }
    path_text(path)?;
    Ok(())
}

/// The root the store records at `path`. No filesystem access.
fn recorded(store: &mut Store, path: &Path) -> Result<Option<RootId>, StoreError> {
    Ok(store.root_at(path)?.map(|row| row.id))
}

/// Records the root at `path`, which the store does not know yet: the one
/// its sentinel named (`identified`), or a new ID, allocated before the
/// sentinel is written so a crash leaves at most a recorded root without
/// one, which initializing again completes. Returns the ID and whether it
/// is new. No filesystem access.
fn claim(
    store: &mut Store,
    path: &Path,
    identified: Option<RootId>,
    registered: bool,
) -> Result<(RootId, bool), RootError> {
    match identified {
        Some(id) => {
            store.adopt_root(id, path, now())?;
            Ok((id, false))
        }
        None => Ok((store.allocate_root(path, registered, now())?, true)),
    }
}

/// Finishes initializing root `id` after its sentinel was written, or
/// forgets a new one whose directory could not be initialized.
/// `registered` marks a root the user chose. No filesystem access.
fn settle(
    store: &mut Store,
    id: RootId,
    allocated: bool,
    registered: bool,
    initialized: Result<(), FsError>,
) -> Result<RootId, RootError> {
    if let Err(e) = initialized {
        if allocated {
            let _ = store.forget_root(id);
        }
        return Err(e.into());
    }
    if registered && !allocated {
        store.register_root(id)?;
    }
    Ok(id)
}

/// Initializes the directory at `path` as a root (see [`claim`]).
/// `registered` marks a root the user chose. Blocking, with the store
/// held throughout: only for `init-root`, while no engine runs.
pub(crate) fn initialize(
    store: &mut Store,
    path: &Path,
    instance: InstanceId,
    registered: bool,
) -> Result<RootId, RootError> {
    check_path(path)?;
    let (id, allocated) = match recorded(store, path)? {
        Some(id) => (id, false),
        None => claim(store, path, Root::identify(path, instance)?, registered)?,
    };
    settle(
        store,
        id,
        allocated,
        registered,
        Root::initialize(path, id, instance),
    )
}

/// Registers the existing directory `path` as a root the user chose, as
/// [`initialize`] does, but with each filesystem step on a blocking thread
/// and the store used only between them, so a hung folder delays only
/// this registration. A folder still being checked or registered is
/// refused as busy.
pub(crate) async fn register(
    store: &StoreClient,
    probes: &Probes,
    path: PathBuf,
    instance: InstanceId,
) -> Result<RootId, ProtocolError> {
    check_path(&path)?;
    let _slot = probes.claim(&path).ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::Busy,
            "the folder is not responding: an earlier check of it has not finished",
        )
    })?;
    let stopped = || ProtocolError::new(ErrorCode::ShuttingDown, "engine is not running");
    let known = {
        let path = path.clone();
        store
            .call(move |store| recorded(store, &path))
            .await
            .ok_or_else(stopped)??
    };
    let (id, allocated) = match known {
        Some(id) => (id, false),
        None => {
            let identified = {
                let path = path.clone();
                blocking(move || Root::identify(&path, instance))
                    .await?
                    .map_err(RootError::from)?
            };
            let path = path.clone();
            store
                .call(move |store| claim(store, &path, identified, true))
                .await
                .ok_or_else(stopped)??
        }
    };
    let initialized = blocking(move || Root::initialize(&path, id, instance)).await?;
    Ok(store
        .call(move |store| settle(store, id, allocated, true, initialized))
        .await
        .ok_or_else(stopped)??)
}

/// Runs filesystem work on a blocking thread.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ProtocolError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ProtocolError::new(ErrorCode::Internal, "the folder could not be checked"))
}

/// The permitted roots and the default, kept by the coordinator.
#[derive(Debug, Clone)]
pub(crate) struct Destinations {
    /// The operator's download root.
    fallback: Option<PathBuf>,
    /// Permitted roots.
    known: BTreeMap<RootId, PathBuf>,
    /// Configured paths not identified yet, in configuration order.
    pending: Vec<PathBuf>,
    /// Settings revision.
    revision: Revision,
    /// The user's chosen default, which may no longer be permitted.
    chosen: Option<RootId>,
}

impl Destinations {
    /// The registered roots and the settings, from the store. Configured
    /// paths are identified by [`Destinations::refresh`]. Blocking.
    pub(crate) fn load(store: &mut Store, config: &EngineConfig) -> Result<Self, StoreError> {
        let (revision, chosen) = store.settings()?;
        let known = store
            .root_rows()?
            .into_iter()
            .filter(|row| row.registered)
            .map(|row| (row.id, row.path))
            .collect();
        Ok(Self {
            fallback: config.download_root.clone(),
            known,
            pending: config.roots.clone(),
            revision,
            chosen,
        })
    }

    /// Whether every configured path is identified.
    pub(crate) fn complete(&self) -> bool {
        self.pending.is_empty()
    }

    /// Identifies configured paths the store already records. A path
    /// being registered is left for later: its record may not have a
    /// sentinel yet. No filesystem access.
    pub(crate) fn recall(&mut self, store: &mut Store, probes: &Probes) {
        let pending = std::mem::take(&mut self.pending);
        for path in pending {
            match recorded(store, &path) {
                Ok(Some(id)) if !probes.busy(&path) => {
                    self.known.insert(id, path);
                }
                _ => self.pending.push(path),
            }
        }
    }

    /// Configured paths not identified yet, to look for sentinels in.
    pub(crate) fn unidentified(&self) -> Vec<PathBuf> {
        self.pending.clone()
    }

    /// Records the root that the sentinel at the configured `path` names.
    /// No filesystem access.
    pub(crate) fn adopt(
        &mut self,
        store: &mut Store,
        path: PathBuf,
        id: RootId,
    ) -> Result<(), StoreError> {
        store.adopt_root(id, &path, now())?;
        self.pending.retain(|p| *p != path);
        self.known.insert(id, path);
        Ok(())
    }

    /// Records a root the host registered.
    pub(crate) fn add(&mut self, id: RootId, path: PathBuf) {
        self.known.insert(id, path);
    }

    /// Whether another root can be permitted while `registering` others
    /// are being registered.
    pub(crate) fn has_room_for(&self, path: &Path, registering: usize) -> bool {
        self.known.values().any(|p| p == path)
            || self.known.len() + self.pending.len() + registering < EngineConfig::MAX_ROOTS
    }

    /// Records the settings after a change.
    pub(crate) fn set_settings(&mut self, revision: Revision, chosen: Option<RootId>) {
        self.revision = revision;
        self.chosen = chosen;
    }

    /// Where newly submitted jobs are saved: the chosen default while it is
    /// permitted, else the operator's download root once identified.
    pub(crate) fn default_root(&self) -> Option<RootId> {
        self.chosen
            .filter(|id| self.known.contains_key(id))
            .or_else(|| self.download_root())
    }

    /// The operator's download root, once identified. A job submitted
    /// without a destination belongs to it.
    pub(crate) fn download_root(&self) -> Option<RootId> {
        let fallback = self.fallback.as_ref()?;
        self.known
            .iter()
            .find(|(_, path)| *path == fallback)
            .map(|(id, _)| *id)
    }

    /// Whether the operator configured a download root.
    pub(crate) fn has_download_root(&self) -> bool {
        self.fallback.is_some()
    }

    /// Whether the operator's download root is configured but not
    /// identified yet: never initialized, or not mounted.
    pub(crate) fn fallback_pending(&self) -> bool {
        self.fallback
            .as_ref()
            .is_some_and(|f| self.pending.contains(f))
    }

    /// Root `id`, if it is permitted.
    pub(crate) fn get(&self, id: RootId) -> Option<Destination> {
        self.known.get(&id).map(|path| Destination {
            id,
            path: path.clone(),
        })
    }

    /// Every permitted root, in ID order.
    pub(crate) fn all(&self) -> Vec<Destination> {
        self.known
            .iter()
            .map(|(id, path)| Destination {
                id: *id,
                path: path.clone(),
            })
            .collect()
    }

    /// The settings, with each root's availability from `probe`.
    pub(crate) fn settings(&self, available: Vec<Result<(), String>>) -> Settings {
        Settings {
            revision: self.revision,
            default_root: self.default_root(),
            roots: self
                .all()
                .into_iter()
                .zip(available)
                .map(|(root, available)| RootSummary {
                    id: root.id,
                    path: root.path.to_string_lossy().into_owned(),
                    available: available.is_ok(),
                    problem: available.err().map(|e| DiagnosticText::new(&e)),
                })
                .collect(),
        }
    }
}

/// Blocking filesystem checks of root folders, at most one running per
/// path: a check that is still running when another is asked for is not
/// repeated, so a hung mount holds one thread, however often it is asked
/// about.
#[derive(Debug, Clone, Default)]
pub(crate) struct Probes(Arc<Mutex<HashSet<PathBuf>>>);

/// A path claimed for filesystem work; dropping it releases the path.
pub(crate) struct Slot(Arc<Mutex<HashSet<PathBuf>>>, PathBuf);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.lock().expect("probe set").remove(&self.1);
    }
}

impl Probes {
    /// Claims `path`, unless work on it is still running.
    pub(crate) fn claim(&self, path: &Path) -> Option<Slot> {
        self.0
            .lock()
            .expect("probe set")
            .insert(path.to_owned())
            .then(|| Slot(self.0.clone(), path.to_owned()))
    }

    /// Whether work on `path` is running.
    pub(crate) fn busy(&self, path: &Path) -> bool {
        self.0.lock().expect("probe set").contains(path)
    }

    /// Runs `work` on `path` on a blocking thread, waiting at most
    /// [`PROBE_TIMEOUT`]. `None` if work on the path was already running,
    /// or this ran out of time; the path stays claimed until it finishes.
    async fn timed<T: Send + 'static>(
        &self,
        path: &Path,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let slot = self.claim(path)?;
        let check = tokio::task::spawn_blocking(move || {
            let result = work();
            drop(slot);
            result
        });
        tokio::time::timeout(PROBE_TIMEOUT, check).await.ok()?.ok()
    }

    /// Whether `root` opens and carries its sentinel.
    pub(crate) async fn check(
        &self,
        root: Destination,
        instance: InstanceId,
    ) -> Result<(), String> {
        let path = root.path.clone();
        self.timed(&path, move || {
            Root::open(&root.path, root.id, instance)
                .map(drop)
                .map_err(|e| unavailable(&e))
        })
        .await
        .unwrap_or_else(|| Err("the folder is not responding".to_owned()))
    }

    /// The root the sentinel at `path` names, `None` if there is none or
    /// the folder could not be read in time.
    pub(crate) async fn identify(&self, path: &Path, instance: InstanceId) -> Option<RootId> {
        let owned = path.to_owned();
        self.timed(path, move || Root::identify(&owned, instance))
            .await?
            .ok()
            .flatten()
    }
}

/// A root's problem in words for its owner.
fn unavailable(err: &FsError) -> String {
    match err {
        FsError::SentinelMissing(_) => {
            "the folder is not initialized, or its drive is not mounted".to_owned()
        }
        FsError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            "the folder is missing".to_owned()
        }
        FsError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied => {
            "Wayfetch may not use the folder".to_owned()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        store: Store,
        base: PathBuf,
        instance: InstanceId,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir(&state).unwrap();
        Fixture {
            store: Store::open(&state).unwrap(),
            base: tmp.path().to_owned(),
            _tmp: tmp,
            instance: InstanceId::generate(),
        }
    }

    /// What the coordinator does: recall recorded paths, then look for
    /// sentinels in the rest.
    fn refresh(destinations: &mut Destinations, f: &mut Fixture) {
        destinations.recall(&mut f.store, &Probes::default());
        for path in destinations.unidentified() {
            if let Ok(Some(id)) = Root::identify(&path, f.instance) {
                destinations.adopt(&mut f.store, path, id).unwrap();
            }
        }
    }

    impl Fixture {
        fn dir(&self, name: &str) -> PathBuf {
            let path = self.base.join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        fn config(&self, roots: &[&Path], download: Option<&Path>) -> EngineConfig {
            let mut config = EngineConfig::new(self.base.join("state"));
            config.roots = roots.iter().map(|p| p.to_path_buf()).collect();
            config.download_root = download.map(Path::to_path_buf);
            config
        }
    }

    #[test]
    fn initializing_twice_keeps_the_id() {
        let mut f = fixture();
        let path = f.dir("media");
        let id = initialize(&mut f.store, &path, f.instance, false).unwrap();
        assert_eq!(
            initialize(&mut f.store, &path, f.instance, false).unwrap(),
            id
        );
        assert_eq!(Root::identify(&path, f.instance).unwrap(), Some(id));
        assert!(matches!(
            initialize(&mut f.store, Path::new("media"), f.instance, false),
            Err(RootError::Relative(_))
        ));
    }

    #[test]
    fn a_directory_that_cannot_be_initialized_leaves_no_root() {
        let mut f = fixture();
        let missing = f.base.join("missing");
        assert!(matches!(
            initialize(&mut f.store, &missing, f.instance, true),
            Err(RootError::Fs(_))
        ));
        assert!(f.store.root_rows().unwrap().is_empty());
        // Another instance's folder is refused too, and left as it was.
        let foreign = f.dir("foreign");
        Root::initialize(
            &foreign,
            RootId::from_u64(7).unwrap(),
            InstanceId::generate(),
        )
        .unwrap();
        assert!(matches!(
            initialize(&mut f.store, &foreign, f.instance, false),
            Err(RootError::Fs(FsError::ForeignSentinel))
        ));
        assert!(f.store.root_rows().unwrap().is_empty());
    }

    #[test]
    fn legacy_roots_are_identified_by_their_sentinel() {
        let mut f = fixture();
        let path = f.dir("downloads");
        let legacy = RootId::from_u64(1).unwrap();
        Root::initialize(&path, legacy, f.instance).unwrap();
        let config = f.config(&[&path], Some(&path));
        let mut destinations = Destinations::load(&mut f.store, &config).unwrap();
        assert_eq!(destinations.default_root(), None);
        refresh(&mut destinations, &mut f);
        assert!(destinations.complete());
        assert_eq!(destinations.default_root(), Some(legacy));
        assert_eq!(f.store.root_at(&path).unwrap().unwrap().id, legacy);
    }

    #[test]
    fn configured_roots_wait_for_initialization() {
        let mut f = fixture();
        let (a, b) = (f.dir("a"), f.dir("b"));
        let config = f.config(&[&a, &b], Some(&a));
        let mut destinations = Destinations::load(&mut f.store, &config).unwrap();
        refresh(&mut destinations, &mut f);
        assert!(!destinations.complete());
        assert!(destinations.all().is_empty());
        let id_b = initialize(&mut f.store, &b, f.instance, false).unwrap();
        refresh(&mut destinations, &mut f);
        assert_eq!(destinations.all().len(), 1);
        // Only the download root is the default before one is chosen.
        assert_eq!(destinations.default_root(), None);
        let id_a = initialize(&mut f.store, &a, f.instance, false).unwrap();
        refresh(&mut destinations, &mut f);
        assert!(destinations.complete());
        assert_eq!(destinations.default_root(), Some(id_a));
        destinations.set_settings(Revision::from_u64(2).unwrap(), Some(id_b));
        assert_eq!(destinations.default_root(), Some(id_b));
    }

    #[test]
    fn only_configured_or_registered_roots_are_permitted() {
        let mut f = fixture();
        let (a, old, chosen) = (f.dir("a"), f.dir("old"), f.dir("chosen"));
        let id_old = initialize(&mut f.store, &old, f.instance, false).unwrap();
        let id_chosen = initialize(&mut f.store, &chosen, f.instance, true).unwrap();
        initialize(&mut f.store, &a, f.instance, false).unwrap();
        let config = f.config(&[&a], Some(&a));
        let mut destinations = Destinations::load(&mut f.store, &config).unwrap();
        refresh(&mut destinations, &mut f);
        // A root the operator stopped configuring is no longer permitted;
        // the one the user registered still is.
        assert!(destinations.get(id_old).is_none());
        assert!(destinations.get(id_chosen).is_some());
        // A chosen default that is no longer permitted falls back.
        destinations.set_settings(Revision::from_u64(2).unwrap(), Some(id_old));
        assert_ne!(destinations.default_root(), Some(id_old));
    }

    #[tokio::test]
    async fn probes_report_problems_and_hold_one_check_per_root() {
        let f = fixture();
        let path = f.dir("probe");
        let root = Destination {
            id: RootId::from_u64(5).unwrap(),
            path: path.clone(),
        };
        let probes = Probes::default();
        let err = probes.check(root.clone(), f.instance).await.unwrap_err();
        assert!(err.contains("not initialized"), "{err}");
        Root::initialize(&path, root.id, f.instance).unwrap();
        probes.check(root.clone(), f.instance).await.unwrap();
        // A check still running is not repeated.
        let slot = probes.claim(&path).unwrap();
        let err = probes.check(root.clone(), f.instance).await.unwrap_err();
        assert!(err.contains("not responding"), "{err}");
        assert_eq!(probes.identify(&path, f.instance).await, None);
        drop(slot);
        assert_eq!(probes.identify(&path, f.instance).await, Some(root.id));
    }
}
