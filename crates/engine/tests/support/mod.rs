//! Driving an engine through its public API in tests: a state directory
//! and download root, an engine started with a given transport, and
//! commands and queries as a host would send them. Shared by the engine's
//! own tests and the HTTP crate's tests over real sockets.

#![allow(dead_code, reason = "each test file uses part of the support")]

use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::runtime::Handle;
use wayfetch_engine::fs::Root;
use wayfetch_engine::transfer::transport::Transport;
use wayfetch_engine::{
    EngineBuilder, EngineConfig, EngineOwner, Limits, RootConfig, ShutdownRequest, Timeouts,
};
use wayfetch_protocol::command::{InputUrl, SubmitItems};
use wayfetch_protocol::domain::id::{JobId, RootId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_protocol::{Command, CommandEnvelope, IdempotencyKey, Permission, Query, QueryResult};

pub const ALL: &[Permission] = &[Permission::Read, Permission::Submit, Permission::Control];
pub const ROOT: RootId = RootId::from_u64(1).unwrap();
pub const KIB: usize = 1024;

/// A state directory, an initialized download root, and the transport
/// engines start with. `server` is whatever the test controls the
/// transport's responses through.
pub struct Env<S> {
    _tmp: tempfile::TempDir,
    pub state: PathBuf,
    pub root: PathBuf,
    pub server: S,
    transport: Arc<dyn Transport>,
    pub limits: Limits,
    pub timeouts: Timeouts,
}

impl<S> Env<S> {
    /// Small read buffers and checkpoints, so transfers of a few KiB pass
    /// through several of each.
    pub fn new(server: S, transport: Arc<dyn Transport>) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (state, root) = (tmp.path().join("state"), tmp.path().join("downloads"));
        std::fs::create_dir(&root).unwrap();
        Self {
            _tmp: tmp,
            state,
            root,
            server,
            transport,
            limits: Limits {
                read_buffer_bytes: NonZeroUsize::new(4 * KIB).unwrap(),
                checkpoint_bytes: NonZeroU64::new(4 * KIB as u64).unwrap(),
                ..Limits::DEFAULT
            },
            timeouts: Timeouts::DEFAULT,
        }
    }

    pub async fn start(&self) -> EngineOwner {
        let mut config = EngineConfig::new(&self.state);
        config.roots.push(RootConfig {
            id: ROOT,
            path: self.root.clone(),
        });
        config.download_root = Some(ROOT);
        config.limits = self.limits.clone();
        config.timeouts = self.timeouts;
        let owner = EngineBuilder::new(config, Handle::current())
            .transport(self.transport.clone())
            .start()
            .await
            .unwrap();
        // An operator action, once per root and instance.
        Root::initialize(&self.root, ROOT, owner.identity().instance).unwrap();
        owner
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
    std::fs::read_dir(root.join(".wayfetch/staging"))
        .unwrap()
        .count()
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
        assert!(
            !matches!(j.stage, JobStage::Failed | JobStage::Blocked),
            "{j:?}"
        );
        j.stage == JobStage::Complete
    })
    .await
}

pub async fn shutdown(owner: EngineOwner) {
    let report = owner.shutdown(ShutdownRequest::default()).await;
    assert!(report.clean, "{report:?}");
}
