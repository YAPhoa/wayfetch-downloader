//! Driving an engine through its public API in tests: a state directory and
//! root, an engine on a given transport, and commands and queries as a host
//! sends them.

#![allow(dead_code, reason = "each test file uses part of the support")]

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::runtime::Handle;
use wayfetch_engine::accounts::provider::AccountProvider;
use wayfetch_engine::crawl::Inspector;
use wayfetch_engine::extract::Extractor;
use wayfetch_engine::fs::Root;
use wayfetch_engine::media::{Converter, MediaService, PluginControl};
use wayfetch_engine::transfer::transport::Transport;
use wayfetch_engine::{
    EngineBuilder, EngineConfig, EngineOwner, Limits, Policy, ShutdownRequest, Timeouts,
};
use wayfetch_protocol::command::{InputUrl, SubmitItems};
use wayfetch_protocol::domain::id::{JobId, RootId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_protocol::{Command, CommandEnvelope, IdempotencyKey, Permission, Query, QueryResult};

pub const ALL: &[Permission] = Permission::ALL;
pub const ROOT: RootId = RootId::from_u64(1).unwrap();
pub const KIB: usize = 1024;
/// The checkpoint interval engines in an [`Env`] use.
pub const CHECKPOINT: usize = 4 * KIB;

/// Whether `durable` bytes are all a transfer stalled at `at` will commit:
/// checkpoints follow the network's chunking, so the last one lands within
/// one checkpoint interval of the stall (the engine may commit the rest
/// while it stops).
pub fn settled(durable: u64, at: usize) -> bool {
    durable + CHECKPOINT as u64 > at as u64
}

/// A state directory, an initialized download root, and the transport
/// engines start with. `server` is whatever the test controls the
/// transport's responses through.
pub struct Env<S> {
    _tmp: Option<tempfile::TempDir>,
    pub state: PathBuf,
    pub root: PathBuf,
    /// Further roots the operator configures; `init_root` initializes one.
    pub roots: Vec<PathBuf>,
    pub server: S,
    transport: Arc<dyn Transport>,
    pub limits: Limits,
    pub timeouts: Timeouts,
    pub policy: Policy,
    /// The credential key file engines are configured with, if any.
    pub credential_key: Option<PathBuf>,
    /// Account integrations engines start with.
    pub providers: Vec<Arc<dyn AccountProvider>>,
    /// Media services engines start with.
    pub media: Vec<Arc<dyn MediaService>>,
    /// The converter engines start with, if any.
    pub converter: Option<Arc<dyn Converter>>,
    /// The archive extractor engines start with, if any.
    pub extractor: Option<Arc<dyn Extractor>>,
    /// What changes plugins, if anything.
    pub plugin_control: Option<Arc<dyn PluginControl>>,
    /// What sends collected links' checks, if anything.
    pub inspector: Option<Arc<dyn Inspector>>,
}

impl<S> Env<S> {
    /// A fresh environment in a temporary directory, removed on drop.
    pub fn new(server: S, transport: Arc<dyn Transport>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (state, root) = (tmp.path().join("state"), tmp.path().join("downloads"));
        std::fs::create_dir(&root).unwrap();
        Self {
            _tmp: Some(tmp),
            ..Self::at(state, root, server, transport)
        }
    }

    /// Engines on an existing state directory and root that another
    /// environment owns and removes, for example in a child process. Small
    /// read buffers and checkpoints, so transfers of a few KiB pass through
    /// several of each.
    pub fn at(state: PathBuf, root: PathBuf, server: S, transport: Arc<dyn Transport>) -> Self {
        Self {
            _tmp: None,
            state,
            root,
            roots: Vec::new(),
            server,
            transport,
            limits: Limits {
                read_buffer_bytes: NonZeroUsize::new(4 * KIB).unwrap(),
                checkpoint_bytes: NonZeroU64::new(CHECKPOINT as u64).unwrap(),
                ..Limits::DEFAULT
            },
            timeouts: Timeouts::DEFAULT,
            policy: Policy::DEFAULT,
            credential_key: None,
            providers: Vec::new(),
            media: Vec::new(),
            converter: None,
            extractor: None,
            plugin_control: None,
            inspector: None,
        }
    }

    pub async fn start(&self) -> EngineOwner {
        let owner = self.start_uninitialized().await;
        // An operator action, once per root and instance. The engine
        // identifies the root by its marker; ID 1 is the one roots of
        // earlier versions carry.
        Root::initialize(&self.root, ROOT, owner.identity().instance).unwrap();
        owner
    }

    /// An engine whose download root is not initialized, as with a drive
    /// not mounted yet.
    pub async fn start_uninitialized(&self) -> EngineOwner {
        let mut config = EngineConfig::new(&self.state);
        config.roots.push(self.root.clone());
        config.roots.extend(self.roots.iter().cloned());
        config.download_root = Some(self.root.clone());
        config.limits = self.limits.clone();
        config.timeouts = self.timeouts;
        config.policy = self.policy;
        config.credential_key = self.credential_key.clone();
        let mut builder =
            EngineBuilder::new(config, Handle::current()).transport(self.transport.clone());
        for provider in &self.providers {
            builder = builder.account_provider(provider.clone());
        }
        for service in &self.media {
            builder = builder.media_service(service.clone());
        }
        if let Some(converter) = &self.converter {
            builder = builder.converter(converter.clone());
        }
        if let Some(extractor) = &self.extractor {
            builder = builder.extractor(extractor.clone());
        }
        if let Some(control) = &self.plugin_control {
            builder = builder.plugin_control(control.clone());
        }
        if let Some(inspector) = &self.inspector {
            builder = builder.inspector(inspector.clone());
        }
        builder.start().await.unwrap()
    }

    /// Initializes `path` as a root of the instance, as `init-root` does,
    /// while no engine runs, and returns its ID.
    pub fn init_root(&self, path: &Path) -> RootId {
        let state = wayfetch_engine::state_dir::StateDir::open(&self.state).unwrap();
        wayfetch_engine::initialize_root(&state, path).unwrap()
    }

    pub fn published(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.root.join(path)).unwrap()
    }

    /// Staging files left in the root.
    pub fn staging_files(&self) -> usize {
        staging_files(&self.root)
    }
}

pub fn staging_files(root: &Path) -> usize {
    std::fs::read_dir(staging_dir(root)).unwrap().count()
}

/// The engine's own staging folder in `root`: the only folder in the
/// root's staging directory, since one engine uses the root.
pub fn staging_dir(root: &Path) -> std::path::PathBuf {
    let mut dirs = std::fs::read_dir(root.join(".wayfetch/staging"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir());
    let dir = dirs.next().expect("a staging folder");
    assert!(dirs.next().is_none(), "one engine's staging folder");
    dir
}

/// Deterministic bytes of `len`.
pub fn body(len: usize) -> Bytes {
    (0..len).map(|i| (i % 251) as u8).collect()
}

pub fn envelope(owner: &EngineOwner, command: Command) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: IdempotencyKey::generate(),
        target: owner.identity().instance,
        command,
    }
}

/// Submits `urls` and returns the new jobs.
pub async fn submit(owner: &EngineOwner, urls: impl IntoIterator<Item = String>) -> Vec<JobId> {
    let before = jobs(owner).await.len();
    let items = urls
        .into_iter()
        .map(|url| InputUrl::parse(&url).unwrap())
        .collect::<Vec<_>>();
    let command = Command::Submit {
        items: SubmitItems::try_from(items).unwrap(),
        package: None,
    };
    owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap();
    jobs(owner).await[before..].iter().map(|j| j.id).collect()
}

pub async fn jobs(owner: &EngineOwner) -> Vec<JobSummary> {
    let query = Query::ListJobs {
        filter: JobFilter::default(),
        after: None,
        page_size: PageSize::DEFAULT,
    };
    match owner.handle().query(query, ALL).await.unwrap() {
        QueryResult::Jobs(page) => page.items,
        other => panic!("{other:?}"),
    }
}

pub async fn job(owner: &EngineOwner, job: JobId) -> JobSummary {
    match owner
        .handle()
        .query(Query::InspectJob { job }, ALL)
        .await
        .unwrap()
    {
        QueryResult::Job { job, .. } => job,
        other => panic!("{other:?}"),
    }
}

/// Polls `job` until `done` holds.
pub async fn wait_for(
    owner: &EngineOwner,
    id: JobId,
    done: impl Fn(&JobSummary) -> bool,
) -> JobSummary {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let summary = job(owner, id).await;
        if done(&summary) {
            return summary;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out: {summary:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub async fn complete(owner: &EngineOwner, id: JobId) -> JobSummary {
    wait_for(owner, id, |j| {
        assert!(j.stage != JobStage::Failed, "{j:?}");
        j.stage == JobStage::Complete
    })
    .await
}

pub async fn shutdown(owner: EngineOwner) {
    let report = owner.shutdown(ShutdownRequest::default()).await;
    assert!(report.clean, "{report:?}");
}
