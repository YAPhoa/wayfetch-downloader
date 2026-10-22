//! Where downloads may be saved: the permitted roots and the default for new
//! jobs (ADR-17, ADR-29).
//!
//! A root is permitted while configured, or after the user registered it
//! explicitly. Jobs and settings name roots by ID, and a job keeps its root:
//! an unmounted or no longer permitted root blocks it rather than
//! redirecting it. Folders may be shared by several profiles. Filesystem
//! checks run off the coordinator with deadlines ([`Probes`]), so a hung
//! mount stalls only requests about it.

use std::any::Any;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::sync::{Notify, Semaphore, watch};
use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::RootId;
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;
use wayfetch_protocol::query::{
    BoundedSetting, FolderListing, MAX_LISTED_FOLDERS, RootSummary, Settings, SpeedLimitSetting,
};
use wayfetch_protocol::{ErrorCode, InstanceId, ProtocolError};

use crate::config::EngineConfig;
use crate::fs::{FsError, Root};
use crate::lifecycle::StoreClient;
use crate::state_dir::StateDir;
use crate::store::{Store, StoreError, path_text};

/// How long an availability check may take before the root is reported as
/// not responding.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many requests about folders the user names (browsing, adding) may
/// have filesystem work running at once, finished or not. Such paths are
/// unbounded, so each holding one thread on a hung mount could otherwise
/// use up the blocking threads.
pub(crate) const MAX_NAMED_FOLDER_WORK: usize = 8;

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
    /// The directory is missing, not accessible, or not a folder.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// The store refused, for example because the folder's marker names a
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
            RootError::Fs(e) => ProtocolError::new(
                ErrorCode::NotApplicable,
                &format!("the folder cannot be used: {}", unavailable(&e)),
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

/// [`initialize_root`], unless the store records that the folder finished
/// initializing: the download root on a first start, or a retry after an
/// attempt failed partway. A finished root is never initialized again here,
/// so one whose mount goes missing stays refused. Returns whether it
/// initialized.
pub fn initialize_new_root(state_dir: &StateDir, path: &Path) -> Result<bool, RootError> {
    let mut store = Store::open(state_dir.path())?;
    initialize_unfinished(&mut store, path, state_dir.identity().instance)
}

fn initialize_unfinished(
    store: &mut Store,
    path: &Path,
    instance: InstanceId,
) -> Result<bool, RootError> {
    check_path(path)?;
    if store.root_initialized(path)? {
        return Ok(false);
    }
    initialize(store, path, instance, false)?;
    Ok(true)
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
/// its marker named (`identified`), or a new ID, allocated before the
/// marker is written so a crash leaves at most a recorded root without
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
            store.adopt_root(id, path, false, now())?;
            Ok((id, false))
        }
        None => Ok((store.allocate_root(path, registered, now())?, true)),
    }
}

/// Finishes initializing root `id` after its marker and folders were
/// written, or forgets a new one whose directory could not be initialized.
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
    store.root_was_initialized(id)?;
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

/// Registers the existing directory `path` as a root the user chose, like
/// [`initialize`] but with filesystem steps on blocking threads, so a hung
/// folder delays only this registration. A folder already being checked is
/// refused as busy, and so is the state directory or a folder inside it.
pub(crate) async fn register(
    store: &StoreClient,
    probes: &Probes,
    cancel: &CancellationToken,
    path: PathBuf,
    instance: InstanceId,
    state_dir: PathBuf,
) -> Result<RootId, ProtocolError> {
    check_path(&path)?;
    let probes = &probes.bounded()?;
    let _slot = probes.claim(&path).ok_or_else(|| {
        ProtocolError::new(
            ErrorCode::Busy,
            "the folder is not responding: an earlier check of it has not finished",
        )
    })?;
    let stopped = || ProtocolError::new(ErrorCode::ShuttingDown, "engine is not running");
    let running = || (!cancel.is_cancelled()).then_some(()).ok_or_else(stopped);
    let inside = {
        let path = path.clone();
        probes.blocking(move || within(&path, &state_dir)).await?
    };
    if inside {
        return Err(ProtocolError::new(
            ErrorCode::InvalidRequest,
            "the folder holds Wayfetch's own state and cannot be a destination",
        ));
    }
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
            running()?;
            let identified = {
                let path = path.clone();
                probes
                    .blocking(move || Root::identify(&path, instance))
                    .await?
                    .map_err(RootError::from)?
            };
            running()?;
            let path = path.clone();
            store
                .call(move |store| claim(store, &path, identified, true))
                .await
                .ok_or_else(stopped)??
        }
    };
    running()?;
    let initialized = probes
        .blocking(move || Root::initialize(&path, id, instance))
        .await?;
    Ok(store
        .call(move |store| settle(store, id, allocated, true, initialized))
        .await
        .ok_or_else(stopped)??)
}

/// Whether `path` is `dir` or inside it, as written or once links are
/// resolved. Blocking.
fn within(path: &Path, dir: &Path) -> bool {
    let resolved = |p: &Path| std::fs::canonicalize(p).ok();
    path.starts_with(dir)
        || matches!((resolved(path), resolved(dir)), (Some(path), Some(dir)) if path.starts_with(&dir))
}

/// The folders inside the folder at `path`, the top of the filesystem if
/// `None`, for the owner to choose one to register. Read on a blocking
/// thread with a deadline, like a root check ([`Probes`]), so a hung mount
/// is reported as not responding.
pub(crate) async fn browse(
    probes: &Probes,
    path: Option<PathBuf>,
) -> Result<FolderListing, ProtocolError> {
    let path = path.unwrap_or_else(|| PathBuf::from("/"));
    if !path.is_absolute() {
        return Err(RootError::Relative(path).into());
    }
    let owned = path.clone();
    probes
        .bounded()?
        .timed(&path, move || list_folders(&owned))
        .await
        .unwrap_or_else(|| {
            Err(ProtocolError::new(
                ErrorCode::Busy,
                "the folder is not responding",
            ))
        })
}

/// [`browse`]'s blocking part. Stops reading after
/// [`MAX_LISTED_FOLDERS`], so a huge folder costs a bounded listing.
fn list_folders(path: &Path) -> Result<FolderListing, ProtocolError> {
    let path = std::fs::canonicalize(path).map_err(|e| unreadable(&e))?;
    let text = |p: &Path| {
        path_text(p).map(str::to_owned).map_err(|_| {
            ProtocolError::new(
                ErrorCode::NotApplicable,
                "the folder's path is not valid UTF-8",
            )
        })
    };
    let mut folders = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(&path).map_err(|e| unreadable(&e))? {
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        // A link to a folder is listed as one.
        let folder = entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()));
        if name.starts_with('.') || !folder {
            continue;
        }
        if folders.len() == MAX_LISTED_FOLDERS {
            truncated = true;
            break;
        }
        folders.push(name);
    }
    folders.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
    Ok(FolderListing {
        parent: path.parent().map(text).transpose()?,
        path: text(&path)?,
        folders,
        truncated,
    })
}

/// Why a folder could not be listed, in words for its owner.
fn unreadable(err: &std::io::Error) -> ProtocolError {
    match err.kind() {
        std::io::ErrorKind::NotFound => {
            ProtocolError::new(ErrorCode::NotFound, "the folder does not exist")
        }
        std::io::ErrorKind::PermissionDenied => {
            ProtocolError::new(ErrorCode::NotApplicable, "Wayfetch may not read the folder")
        }
        std::io::ErrorKind::NotADirectory => {
            ProtocolError::new(ErrorCode::NotApplicable, "that is not a folder")
        }
        _ => ProtocolError::new(
            ErrorCode::NotApplicable,
            &format!("the folder cannot be read: {err}"),
        ),
    }
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
    /// Every path the operator configured, the download root included:
    /// permitted while configured, unless the user removed it.
    configured: Vec<PathBuf>,
    /// Settings revision.
    revision: Revision,
    /// The user's chosen default, which may no longer be permitted.
    chosen: Option<RootId>,
}

impl Destinations {
    /// The registered roots and the settings, from the store. Configured
    /// paths are identified by [`Destinations::refresh`]. Blocking.
    pub(crate) fn load(store: &mut Store, config: &EngineConfig) -> Result<Self, StoreError> {
        let saved = store.settings()?;
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
            configured: config
                .roots
                .iter()
                .chain(&config.download_root)
                .cloned()
                .collect(),
            revision: saved.revision,
            chosen: saved.default_root,
        })
    }

    /// Whether every configured path is identified.
    pub(crate) fn complete(&self) -> bool {
        self.pending.is_empty()
    }

    /// Identifies configured paths the store already records, and drops
    /// those the user removed. A path being registered is left for later:
    /// its record may not have a marker yet. No filesystem access.
    pub(crate) fn recall(&mut self, store: &mut Store, probes: &Probes) {
        let pending = std::mem::take(&mut self.pending);
        for path in pending {
            match store.root_at(&path) {
                Ok(Some(row)) if !probes.busy(&path) => {
                    if !row.removed {
                        self.known.insert(row.id, path);
                    }
                }
                _ => self.pending.push(path),
            }
        }
    }

    /// Configured paths not identified yet, to look for markers in.
    pub(crate) fn unidentified(&self) -> Vec<PathBuf> {
        self.pending.clone()
    }

    /// Records the root that the marker at the configured `path` names.
    /// No filesystem access.
    pub(crate) fn adopt(
        &mut self,
        store: &mut Store,
        path: PathBuf,
        id: RootId,
    ) -> Result<(), StoreError> {
        // Identified by its marker, so it was set up and may hold jobs'
        // files: never initialized again by a first-start setup.
        store.adopt_root(id, &path, true, now())?;
        self.pending.retain(|p| *p != path);
        self.known.insert(id, path);
        Ok(())
    }

    /// Records a root the host registered.
    pub(crate) fn add(&mut self, id: RootId, path: PathBuf) {
        self.known.insert(id, path);
    }

    /// Whether permitted root `id` was added by the user rather than
    /// configured.
    pub(crate) fn added(&self, id: RootId) -> bool {
        self.known
            .get(&id)
            .is_some_and(|path| !self.configured.contains(path))
    }

    /// Forgets a root the user removed.
    pub(crate) fn remove(&mut self, id: RootId) {
        self.known.remove(&id);
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
    pub(crate) fn settings(
        &self,
        available: Vec<Result<(), String>>,
        max_active_downloads: BoundedSetting,
        download_speed_limit: SpeedLimitSetting,
        plugins: Vec<wayfetch_protocol::media::PluginSummary>,
        switches: Switches,
    ) -> Settings {
        Settings {
            extract_archives: switches.extract_archives,
            automatic_update_checks: switches.automatic_update_checks,
            check_files: switches.check_files,
            redownload_failed_checks: switches.redownload_failed_checks,
            max_active_downloads,
            download_speed_limit,
            plugins,
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
                    added: self.added(root.id),
                })
                .collect(),
        }
    }
}

/// Keys of work in progress, at most one piece of work per key: one
/// that is still running when another is asked for is refused, or waited
/// for ([`Claims::claim_within`]), never run alongside it.
#[derive(Debug)]
pub(crate) struct Claims<K>(Arc<Held<K>>);

/// The claimed keys, and a signal each time one is released.
#[derive(Debug)]
struct Held<K> {
    keys: Mutex<HashSet<K>>,
    released: Notify,
}

impl<K> Clone for Claims<K> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<K> Default for Claims<K> {
    fn default() -> Self {
        Self(Arc::new(Held {
            keys: Mutex::default(),
            released: Notify::new(),
        }))
    }
}

/// A claimed key; dropping it releases the key.
pub(crate) struct Slot<K: Hash + Eq>(Arc<Held<K>>, K);

impl<K: Hash + Eq> Drop for Slot<K> {
    fn drop(&mut self) {
        self.0.keys.lock().expect("claim set").remove(&self.1);
        self.0.released.notify_waiters();
    }
}

impl<K: Hash + Eq + Clone> Claims<K> {
    /// Claims `key`, unless work on it is still running.
    pub(crate) fn claim(&self, key: &K) -> Option<Slot<K>> {
        self.0
            .keys
            .lock()
            .expect("claim set")
            .insert(key.clone())
            .then(|| Slot(self.0.clone(), key.clone()))
    }

    /// Claims `key` once work on it ends, waiting at most `wait`: `None`
    /// if it is still running then.
    pub(crate) async fn claim_within(&self, key: &K, wait: Duration) -> Option<Slot<K>> {
        tokio::time::timeout(wait, async {
            loop {
                // Registered before trying, so a release in between is seen.
                let released = self.0.released.notified();
                if let Some(slot) = self.claim(key) {
                    return slot;
                }
                released.await;
            }
        })
        .await
        .ok()
    }

    /// Whether work on `key` is running.
    pub(crate) fn busy(&self, key: &K) -> bool {
        self.0.keys.lock().expect("claim set").contains(key)
    }
}

/// Availability checks under way, by path: each receiver gets the check's
/// answer once it ends.
type Checks = Mutex<HashMap<PathBuf, watch::Receiver<Option<Result<(), String>>>>>;

/// Blocking filesystem checks of root folders, at most one running per
/// path: a check that is still running when another is asked for is not
/// repeated, so a hung mount holds one thread, however often it is asked
/// about.
///
/// Work on folders the user names is also bounded overall
/// ([`Probes::bounded`]), since their paths are not.
#[derive(Debug, Clone)]
pub(crate) struct Probes {
    running: Claims<PathBuf>,
    /// Availability checks under way, by path, for later ones to share.
    checks: Arc<Checks>,
    /// Held by every check until it ends, so the state directory stays
    /// locked while one may still touch a folder.
    guard: Arc<dyn Any + Send + Sync>,
    /// Permits for work on folders the user names.
    named: Arc<Semaphore>,
}

impl Default for Probes {
    fn default() -> Self {
        Self::new(Arc::new(()))
    }
}

impl Probes {
    /// Checks that hold `guard` while they run.
    pub(crate) fn new(guard: Arc<dyn Any + Send + Sync>) -> Self {
        Self {
            running: Claims::default(),
            checks: Arc::default(),
            guard,
            named: Arc::new(Semaphore::new(MAX_NAMED_FOLDER_WORK)),
        }
    }

    /// Probes for one request about a folder the user named, holding one
    /// of [`MAX_NAMED_FOLDER_WORK`] permits until every piece of work they
    /// start has ended, even work whoever awaited it gave up on. Refused
    /// as busy when none is free.
    pub(crate) fn bounded(&self) -> Result<Self, ProtocolError> {
        let permit = self.named.clone().try_acquire_owned().map_err(|_| {
            ProtocolError::new(
                ErrorCode::Busy,
                "too many folders are still being read or are not responding; try again later",
            )
        })?;
        Ok(Self {
            guard: Arc::new((self.guard.clone(), permit)),
            ..self.clone()
        })
    }

    /// Claims `path`, unless work on it is still running.
    pub(crate) fn claim(&self, path: &Path) -> Option<Slot<PathBuf>> {
        self.running.claim(&path.to_owned())
    }

    /// Claims `path` once work on it ends, waiting at most
    /// [`PROBE_TIMEOUT`]: `None` if it is still running then, as on a
    /// folder that does not respond.
    pub(crate) async fn claim_soon(&self, path: &Path) -> Option<Slot<PathBuf>> {
        self.running
            .claim_within(&path.to_owned(), PROBE_TIMEOUT)
            .await
    }

    /// Whether work on `path` is running.
    pub(crate) fn busy(&self, path: &Path) -> bool {
        self.running.busy(&path.to_owned())
    }

    /// Runs filesystem work on a blocking thread, holding the guard until
    /// it ends, even if whoever awaits it gives up.
    pub(crate) fn blocking<T, F>(
        &self,
        work: F,
    ) -> impl Future<Output = Result<T, ProtocolError>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let guard = self.guard.clone();
        let running = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            work()
        });
        async {
            running.await.map_err(|_| {
                ProtocolError::new(ErrorCode::Internal, "the folder could not be checked")
            })
        }
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
        let check = self.blocking(move || {
            let result = work();
            drop(slot);
            result
        });
        tokio::time::timeout(PROBE_TIMEOUT, check).await.ok()?.ok()
    }

    /// Whether `root` opens and carries this profile's marker. While another check
    /// of the same path runs, its answer is shared.
    pub(crate) async fn check(
        &self,
        root: Destination,
        instance: InstanceId,
    ) -> Result<(), String> {
        const NOT_RESPONDING: &str = "the folder is not responding";
        let path = root.path.clone();
        let (answer, mut waiting) = {
            let mut checks = self.checks.lock().unwrap_or_else(|e| e.into_inner());
            match checks.get(&path) {
                Some(running) => (None, running.clone()),
                None => {
                    let (answer, running) = watch::channel(None);
                    checks.insert(path.clone(), running.clone());
                    (Some(answer), running)
                }
            }
        };
        let Some(answer) = answer else {
            return match waiting.wait_for(Option::is_some).await {
                Ok(answer) => answer
                    .clone()
                    .unwrap_or_else(|| Err(NOT_RESPONDING.to_owned())),
                // The check was given up on before it answered.
                Err(_) => Err(NOT_RESPONDING.to_owned()),
            };
        };
        // Forgotten when this check ends, or is given up on.
        let _running = Running {
            checks: &self.checks,
            path: &path,
        };
        let result = self
            .timed(&path, move || {
                Root::open(&root.path, root.id, instance)
                    .map(drop)
                    .map_err(|e| unavailable(&e))
            })
            .await
            .unwrap_or_else(|| Err(NOT_RESPONDING.to_owned()));
        answer.send_replace(Some(result.clone()));
        result
    }

    /// The root this profile's marker at `path` names, `None` if there is none or
    /// the folder could not be read in time.
    pub(crate) async fn identify(&self, path: &Path, instance: InstanceId) -> Option<RootId> {
        let owned = path.to_owned();
        self.timed(path, move || Root::identify(&owned, instance))
            .await?
            .ok()
            .flatten()
    }
}

/// An availability check under way, forgotten when dropped.
struct Running<'a> {
    checks: &'a Checks,
    path: &'a Path,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.checks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.path);
    }
}

/// A root's problem in words for its owner.
fn unavailable(err: &FsError) -> String {
    match err {
        FsError::MarkerMissing(_) => {
            "the folder is not initialized, or its drive is not mounted".to_owned()
        }
        FsError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            "the folder is missing".to_owned()
        }
        FsError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied => {
            "Wayfetch may not use the folder".to_owned()
        }
        FsError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotADirectory => {
            "that is not a folder".to_owned()
        }
        other => other.to_string(),
    }
}

/// The settings that are on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Switches {
    /// Whether downloaded archives are unpacked as they finish.
    pub(crate) extract_archives: bool,
    /// Whether the engine checks for a newer release on its own.
    pub(crate) automatic_update_checks: bool,
    /// Whether complete downloads are checked against checksums.
    pub(crate) check_files: bool,
    /// Whether a download that fails its check is downloaded again once.
    pub(crate) redownload_failed_checks: bool,
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
    /// markers in the rest.
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
    fn a_setup_that_failed_partway_is_retried_but_a_finished_one_never_is() {
        let mut f = fixture();
        let path = f.dir("downloads");
        // The marker can be written, but not the staging folder.
        let private = path.join(crate::fs::PRIVATE_DIR);
        std::fs::create_dir(&private).unwrap();
        let blocker = private.join("staging");
        std::fs::write(&blocker, b"x").unwrap();
        for _ in 0..2 {
            assert!(initialize_unfinished(&mut f.store, &path, f.instance).is_err());
        }
        // The second attempt adopted the first one's marker and kept it.
        assert!(f.store.root_at(&path).unwrap().is_some());
        assert!(!f.store.root_initialized(&path).unwrap());

        std::fs::remove_file(&blocker).unwrap();
        assert!(initialize_unfinished(&mut f.store, &path, f.instance).unwrap());
        assert!(f.store.root_initialized(&path).unwrap());
        assert!(blocker.is_dir());

        // Finished: an empty folder in its place, such as a missing mount,
        // is left alone.
        std::fs::rename(&path, f.base.join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!initialize_unfinished(&mut f.store, &path, f.instance).unwrap());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
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
        let err =
            ProtocolError::from(initialize(&mut f.store, &missing, f.instance, true).unwrap_err());
        assert!(
            err.message.as_str().contains("the folder is missing"),
            "{err:?}"
        );
        // Nothing was created in its place.
        assert!(!missing.exists());
        let file = f.base.join("file");
        std::fs::write(&file, b"x").unwrap();
        let err =
            ProtocolError::from(initialize(&mut f.store, &file, f.instance, true).unwrap_err());
        assert!(err.message.as_str().contains("not a folder"), "{err:?}");
        assert!(f.store.root_rows().unwrap().is_empty());
    }

    #[test]
    fn a_folder_another_profile_uses_can_be_added() {
        let mut f = fixture();
        let shared = f.dir("shared");
        let theirs = InstanceId::generate();
        Root::initialize(&shared, RootId::from_u64(7).unwrap(), theirs).unwrap();
        let id = initialize(&mut f.store, &shared, f.instance, true).unwrap();
        assert_eq!(Root::identify(&shared, f.instance).unwrap(), Some(id));
        assert_eq!(
            Root::identify(&shared, theirs).unwrap(),
            RootId::from_u64(7)
        );
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

    #[tokio::test]
    async fn a_check_asked_for_while_another_runs_shares_its_answer() {
        let f = fixture();
        let path = f.dir("shared");
        let root = Destination {
            id: RootId::from_u64(6).unwrap(),
            path: path.clone(),
        };
        let probes = Probes::default();
        // A check of the root is under way.
        let (answer, running) = watch::channel(None);
        probes.checks.lock().unwrap().insert(path.clone(), running);
        let waiting = tokio::spawn({
            let (probes, root) = (probes.clone(), root.clone());
            async move { probes.check(root, f.instance).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        answer.send_replace(Some(Err("shared".to_owned())));
        assert_eq!(waiting.await.unwrap(), Err("shared".to_owned()));
        // One given up on answers that it is not responding.
        let (answer, running) = watch::channel(None);
        probes.checks.lock().unwrap().insert(path.clone(), running);
        let waiting = tokio::spawn({
            let (probes, root) = (probes.clone(), root.clone());
            async move { probes.check(root, f.instance).await }
        });
        drop(answer);
        assert!(
            waiting
                .await
                .unwrap()
                .unwrap_err()
                .contains("not responding")
        );
        // Once it ends, the next check runs afresh.
        probes.checks.lock().unwrap().clear();
        let err = probes.check(root, f.instance).await.unwrap_err();
        assert!(err.contains("not initialized"), "{err}");
        assert!(probes.checks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn work_on_named_folders_is_bounded_until_it_ends() {
        let f = fixture();
        let probes = Probes::default();
        // Browsing a hung folder gives up at the deadline, but its work
        // keeps a permit until it ends.
        let (release, hung) = std::sync::mpsc::channel::<()>();
        let hung = Arc::new(Mutex::new(hung));
        let mut held = Vec::new();
        for _ in 0..MAX_NAMED_FOLDER_WORK {
            let bounded = probes.bounded().unwrap();
            let hung = hung.clone();
            held.push(bounded.blocking(move || {
                let _ = hung.lock().unwrap().recv();
            }));
        }
        let path = f.dir("browsed");
        let err = browse(&probes, Some(path.clone())).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Busy);
        // Giving up on the work does not free its permit.
        drop(held);
        assert_eq!(
            browse(&probes, Some(path.clone())).await.unwrap_err().code,
            ErrorCode::Busy
        );
        for _ in 0..MAX_NAMED_FOLDER_WORK {
            release.send(()).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while probes.named.available_permits() < MAX_NAMED_FOLDER_WORK {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        browse(&probes, Some(path)).await.unwrap();
    }
}
