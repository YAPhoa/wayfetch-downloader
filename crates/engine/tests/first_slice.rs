//! The first end-to-end slice (architecture brief section 14.3) through the
//! public engine API: submit one direct link, persist the job, download it
//! into owned staging, verify, publish, restart the engine, and list the
//! completed artifact.
//!
//! Bytes come from an in-process [`Transport`]; `wayfetch-net` runs the
//! same path over real sockets. Brief tests exercised here at the
//! engine level: T01 (sequential file), T02 and T23 (resume from durable
//! checkpoints after a restart), T03 (`200` to a range request), T05
//! (representation changed during resume), T09 (stalled headers and body),
//! T27 (cancelled generation), and T47 (bounded subscriber replay). The
//! last tests cover destination roots and the default for new jobs,
//! renaming and removing finished jobs, and links collected for review
//! before they are started.

mod support;

use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, KIB, ROOT, body, complete, envelope, job, jobs, shutdown, wait_for};
use wayfetch_engine::fs::Root;
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_engine::{EngineOwner, IntegerPolicy, SpeedLimitPolicy, Timeouts};
use wayfetch_protocol::command::{
    AppliedLink, CollectMode, CollectText, LinkRef, LinkRefs, MAX_COLLECTED_LINKS,
    MAX_SUBMIT_ITEMS, PreferenceChange, SpeedLimitChange,
};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::{JobId, RootId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::version::Revision;
use wayfetch_protocol::event::{EventKind, ResyncReason};
use wayfetch_protocol::query::{
    BoundedSetting, CollectedLink, JobFilter, JobSummary, LinkCheck, LinkPage, PageSize,
    SettingSource, Settings, SpeedLimit, SpeedLimitSetting,
};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, Permission, ProtocolError, Query,
    QueryResult, StreamItem, Subscribe,
};

/// A file the fake server holds.
#[derive(Debug, Clone)]
struct Resource {
    body: Bytes,
    etag: Option<String>,
    /// Whether `Range` is honored at all.
    ranges: bool,
    /// Whether `If-Range` is honored; a server that ignores it sends a
    /// changed representation's bytes for the old range.
    if_range: bool,
    /// The body stops sending at this offset until cleared.
    stall_at: Option<usize>,
    /// Response headers never arrive.
    no_headers: bool,
    filename: Option<String>,
    /// How a range request is answered instead of honoring it.
    range_reply: Option<RangeReply>,
}

/// An unusable answer to a range request.
#[derive(Debug, Clone, Copy)]
enum RangeReply {
    /// `206` starting a few bytes before the requested start.
    WrongStart,
    /// `416` reporting the full length.
    Unsatisfiable,
}

impl Resource {
    fn new(body: Bytes) -> Self {
        Self {
            body,
            etag: Some("\"v1\"".into()),
            ranges: true,
            if_range: true,
            stall_at: None,
            no_headers: false,
            filename: None,
            range_reply: None,
        }
    }

    fn representation(&self) -> Representation {
        Representation {
            length: Some(self.body.len() as u64),
            strong_etag: self.etag.clone(),
            last_modified: None,
            resource: None,
        }
    }
}

/// An in-process server: resources by URL path, and a log of requests.
#[derive(Debug, Default)]
struct Server {
    resources: Mutex<HashMap<String, Resource>>,
    requests: Mutex<Vec<OpenRequest>>,
    open_bodies: AtomicUsize,
    most_open_bodies: AtomicUsize,
    /// Body bytes handed to the engine, by URL path.
    served: Mutex<HashMap<String, usize>>,
}

impl Server {
    fn put(&self, path: &str, resource: Resource) {
        self.resources
            .lock()
            .unwrap()
            .insert(path.to_owned(), resource);
    }

    fn update(&self, path: &str, change: impl FnOnce(&mut Resource)) {
        change(self.resources.lock().unwrap().get_mut(path).unwrap());
    }

    fn stall_at(&self, path: &str) -> Option<usize> {
        self.resources.lock().unwrap()[path].stall_at
    }

    fn served(&self, path: &str) -> usize {
        self.served.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    fn served_total(&self) -> usize {
        self.served.lock().unwrap().values().sum()
    }

    fn ranges_requested(&self) -> Vec<Option<(u64, Option<u64>)>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.range)
            .collect()
    }
}

struct Body {
    server: Arc<Server>,
    path: String,
    data: Bytes,
    position: usize,
    chunk: usize,
}

impl Drop for Body {
    fn drop(&mut self) {
        self.server.open_bodies.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
        Box::pin(async move {
            // `data` starts at the response's first byte; stalls are by
            // absolute offset.
            loop {
                let offset = self.body_offset();
                match self.server.stall_at(&self.path) {
                    Some(at) if offset >= at => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    stall => {
                        if self.position == self.data.len() {
                            return Ok(None);
                        }
                        let mut end = (self.position + self.chunk).min(self.data.len());
                        if let Some(at) = stall {
                            end = end.min(self.position + (at - offset));
                        }
                        let chunk = self.data.slice(self.position..end);
                        self.position = end;
                        *self
                            .server
                            .served
                            .lock()
                            .unwrap()
                            .entry(self.path.clone())
                            .or_default() += chunk.len();
                        return Ok(Some(chunk));
                    }
                }
            }
        })
    }
}

impl Body {
    fn body_offset(&self) -> usize {
        let full = self.server.resources.lock().unwrap()[&self.path].body.len();
        full - self.data.len() + self.position
    }
}

/// The transport handed to the engine; the test keeps the server.
struct Fake(Arc<Server>);

impl Transport for Fake {
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        Box::pin(async move {
            let server = &self.0;
            server.requests.lock().unwrap().push(request.clone());
            let path = request.url.path().to_owned();
            let Some(resource) = server.resources.lock().unwrap().get(&path).cloned() else {
                return Err(TransportError {
                    code: FailureCode::ResourceDeleted,
                    message: "404".into(),
                });
            };
            if resource.no_headers {
                std::future::pending::<()>().await;
            }
            let len = resource.body.len() as u64;
            let representation = resource.representation();
            let matches = request
                .validators
                .as_ref()
                .is_none_or(|v| *v == representation || !resource.if_range);
            let (framing, start) = match (request.range, resource.range_reply) {
                (Some((start, None)), Some(RangeReply::WrongStart)) => (
                    Framing::Partial {
                        first: start - 1,
                        last: len - 1,
                        complete: Some(len),
                    },
                    start - 1,
                ),
                (Some(_), Some(RangeReply::Unsatisfiable)) => (
                    Framing::Unsatisfiable {
                        complete: Some(len),
                    },
                    len,
                ),
                (Some((start, None)), None) if resource.ranges && matches => (
                    Framing::Partial {
                        first: start,
                        last: len - 1,
                        complete: Some(len),
                    },
                    start,
                ),
                _ => (Framing::Full { length: Some(len) }, 0),
            };
            let open = server.open_bodies.fetch_add(1, Ordering::SeqCst) + 1;
            server.most_open_bodies.fetch_max(open, Ordering::SeqCst);
            Ok(Response {
                framing,
                representation,
                filename: resource.filename.clone(),
                body: Box::new(Body {
                    server: server.clone(),
                    path,
                    data: resource.body.slice(usize::try_from(start).unwrap()..),
                    position: 0,
                    chunk: request.read_buffer,
                }),
            })
        })
    }
}

type Env = support::Env<Arc<Server>>;

/// An environment whose engines download from a fresh fake server.
fn env() -> Env {
    let server = Arc::<Server>::default();
    Env::new(server.clone(), Arc::new(Fake(server)))
}

/// Submits `paths` on the fake server and returns the new jobs.
async fn submit(owner: &EngineOwner, paths: &[&str]) -> Vec<JobId> {
    support::submit(
        owner,
        paths.iter().map(|p| format!("https://files.example{p}")),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_download_is_published_and_listed_after_restart() {
    let env = env();
    let data = body(50 * KIB + 7);
    env.server
        .put("/games/My%20Game.rar", Resource::new(data.clone()));
    let owner = env.start().await;
    let [id] = submit(&owner, &["/games/My%20Game.rar"]).await[..] else {
        panic!()
    };
    let done = complete(&owner, id).await;
    assert_eq!(done.bytes_total, Some(data.len() as u64));
    assert_eq!(done.bytes_done, data.len() as u64);
    let artifact = done.artifact.clone().expect("published");
    assert_eq!(
        (artifact.root, artifact.path.as_str(), artifact.size),
        (ROOT, "My Game.rar", data.len() as u64)
    );
    assert_eq!(env.published("My Game.rar"), data);
    // Published by linking: no staging name is left behind.
    assert_eq!(env.staging_files(), 0);
    assert_eq!(env.server.ranges_requested(), vec![None]);
    shutdown(owner).await;

    let owner = env.start().await;
    let listed = jobs(&owner).await;
    assert_eq!(listed, vec![done]);
    let completed = Query::ListJobs {
        filter: JobFilter {
            stages: vec![JobStage::Complete],
            package: None,
        },
        after: None,
        page_size: PageSize::DEFAULT,
    };
    let QueryResult::Jobs(page) = owner.handle().query(completed, ALL).await.unwrap() else {
        panic!()
    };
    assert_eq!(page.items[0].artifact, Some(artifact));
    assert_eq!(env.published("My Game.rar"), data);
    // Nothing was requested again.
    assert_eq!(env.server.requests.lock().unwrap().len(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_empty_file_and_a_taken_name_publish() {
    let env = env();
    std::fs::write(env.root.join("empty.bin"), b"someone else's").unwrap();
    env.server.put("/empty.bin", Resource::new(Bytes::new()));
    let owner = env.start().await;
    let [id] = submit(&owner, &["/empty.bin"]).await[..] else {
        panic!()
    };
    let done = complete(&owner, id).await;
    // The existing file is untouched; the artifact takes the next name.
    assert_eq!(done.artifact.unwrap().path, "empty (1).bin");
    assert_eq!(env.published("empty (1).bin"), b"");
    assert_eq!(env.published("empty.bin"), b"someone else's");
    shutdown(owner).await;
}

/// Starts a download that stalls at `at` bytes, waits until they are
/// durable, and stops the engine as a crash would leave it at its last
/// checkpoint.
async fn interrupt_at(env: &Env, path: &str, data: &Bytes, at: usize) -> JobId {
    let mut resource = Resource::new(data.clone());
    resource.stall_at = Some(at);
    env.server.put(path, resource);
    let owner = env.start().await;
    let [id] = submit(&owner, &[path]).await[..] else {
        panic!()
    };
    wait_for(&owner, id, |j| j.bytes_done == at as u64).await;
    shutdown(owner).await;
    env.server.update(path, |r| r.stall_at = None);
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_download_resumes_from_durable_bytes_after_restart() {
    let env = env();
    let data = body(100 * KIB);
    let id = interrupt_at(&env, "/big.bin", &data, 40 * KIB).await;

    let owner = env.start().await;
    let done = complete(&owner, id).await;
    // Recovery moved the job to a new generation that adopted the partial
    // and asked only for the rest, conditioned on the same representation.
    assert!(done.generation.get() > 1);
    assert_eq!(
        env.server.ranges_requested(),
        vec![None, Some((40 * KIB as u64, None))]
    );
    let resumed = env.server.requests.lock().unwrap()[1].clone();
    assert_eq!(
        resumed.validators.unwrap().strong_etag.as_deref(),
        Some("\"v1\"")
    );
    assert_eq!(env.published("big.bin"), data);
    assert_eq!(env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_the_server_does_not_honor_restarts_from_zero() {
    type Change = fn(&mut Resource);
    let cases: [(&str, Change); 3] = [
        // T03: `200` to a range request is never appended.
        ("ignores-range", |r| r.ranges = false),
        // T05: the resource changed; `If-Range` answers with all of it.
        ("changed", |r| r.etag = Some("\"v2\"".into())),
        // T05: the resource changed and the server ignores `If-Range`,
        // sending the new representation's bytes for the old range.
        ("changed-ignoring-if-range", |r| {
            r.etag = Some("\"v2\"".into());
            r.if_range = false;
        }),
    ];
    for (name, change) in cases {
        let env = env();
        let path = format!("/{name}.bin");
        let data = body(64 * KIB);
        let id = interrupt_at(&env, &path, &data, 20 * KIB).await;
        env.server.update(&path, change);

        let owner = env.start().await;
        let done = complete(&owner, id).await;
        assert_eq!(
            env.server.ranges_requested(),
            vec![None, Some((20 * KIB as u64, None)), None],
            "{name}"
        );
        // The artifact is the whole representation from one response, not
        // old bytes with new ones appended.
        assert_eq!(env.published(&format!("{name}.bin")), data, "{name}");
        assert_eq!(done.artifact.unwrap().size, data.len() as u64);
        // The abandoned partial was removed before the new generation ran.
        assert_eq!(env.staging_files(), 0, "{name}");
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_headers_and_bodies_fail_at_their_deadlines() {
    let mut env = env();
    env.timeouts = Timeouts {
        connect: Duration::from_millis(50),
        response: Duration::from_millis(100),
        idle_read: Duration::from_millis(100),
    };
    let mut no_headers = Resource::new(body(KIB));
    no_headers.no_headers = true;
    env.server.put("/no-headers.bin", no_headers);
    let mut stalled = Resource::new(body(16 * KIB));
    stalled.stall_at = Some(8 * KIB);
    env.server.put("/stalled.bin", stalled);
    let owner = env.start().await;
    let ids = submit(&owner, &["/no-headers.bin", "/stalled.bin"]).await;
    for id in ids {
        let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
        let failure = failed.failure.unwrap();
        assert_eq!(failure.code, FailureCode::RequestTimeout);
        assert!(failed.artifact.is_none());
    }
    // A failed transfer frees its slot and its response.
    assert_eq!(env.server.open_bodies.load(Ordering::SeqCst), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_keeps_durable_bytes_and_cancel_retires_the_generation() {
    let env = env();
    let data = body(32 * KIB);
    for path in ["/paused.bin", "/cancelled.bin"] {
        let mut resource = Resource::new(data.clone());
        resource.stall_at = Some(12 * KIB);
        env.server.put(path, resource);
    }
    let owner = env.start().await;
    let ids = submit(&owner, &["/paused.bin", "/cancelled.bin"]).await;
    let (paused, cancelled) = (ids[0], ids[1]);
    for id in ids {
        wait_for(&owner, id, |j| j.bytes_done == 12 * KIB as u64).await;
    }
    let handle = owner.handle();

    let summary = job(&owner, paused).await;
    let pause = Command::Pause {
        job: paused,
        expected_revision: summary.revision,
    };
    handle.execute(envelope(&owner, pause), ALL).await.unwrap();
    let summary = wait_for(&owner, paused, |j| j.stage == JobStage::Paused).await;
    assert_eq!(summary.bytes_done, 12 * KIB as u64);

    let summary = job(&owner, cancelled).await;
    let cancel = Command::Cancel {
        job: cancelled,
        expected_revision: summary.revision,
    };
    handle.execute(envelope(&owner, cancel), ALL).await.unwrap();
    let summary = wait_for(&owner, cancelled, |j| j.stage == JobStage::Cancelled).await;
    assert!(summary.generation > job(&owner, paused).await.generation);
    // Both transfers released their responses.
    assert_eq!(env.server.open_bodies.load(Ordering::SeqCst), 0);

    // Resuming continues the paused generation's partial.
    env.server.update("/paused.bin", |r| r.stall_at = None);
    let summary = job(&owner, paused).await;
    let resume = Command::Resume {
        job: paused,
        expected_revision: summary.revision,
    };
    handle.execute(envelope(&owner, resume), ALL).await.unwrap();
    let done = complete(&owner, paused).await;
    assert_eq!(done.generation, summary.generation);
    assert_eq!(env.published("paused.bin"), data);
    let ranges = env.server.ranges_requested();
    assert_eq!(ranges.len(), 3);
    assert_eq!(ranges[2], Some((12 * KIB as u64, None)));
    // The cancelled job stays cancelled and published nothing.
    assert_eq!(job(&owner, cancelled).await.stage, JobStage::Cancelled);
    assert!(!env.root.join("cancelled.bin").exists());
    // Cancelling retains its partial (section 7.2), across a restart too.
    assert_eq!(env.staging_files(), 1);
    shutdown(owner).await;
    let owner = env.start().await;
    assert_eq!(job(&owner, cancelled).await.bytes_done, 12 * KIB as u64);
    assert_eq!(env.staging_files(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn max_active_downloads_bounds_concurrent_transfers() {
    let mut env = env();
    env.policy.max_active_downloads.default = Some(2);
    let paths = ["/a.bin", "/b.bin", "/c.bin", "/d.bin", "/e.bin"];
    for path in paths {
        let mut resource = Resource::new(body(8 * KIB));
        resource.stall_at = Some(4 * KIB);
        env.server.put(path, resource);
    }
    let owner = env.start().await;
    let ids = submit(&owner, &paths).await;
    wait_for(&owner, ids[1], |j| j.bytes_done > 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Two run; the rest wait in the database, not in memory.
    assert_eq!(env.server.requests.lock().unwrap().len(), 2);
    for id in &ids[2..] {
        assert_eq!(job(&owner, *id).await.stage, JobStage::Queued);
    }
    for path in paths {
        env.server.update(path, |r| r.stall_at = None);
    }
    for id in ids {
        complete(&owner, id).await;
    }
    assert_eq!(env.server.most_open_bodies.load(Ordering::SeqCst), 2);
    shutdown(owner).await;
}

fn max_active(
    owner: &EngineOwner,
    revision: Revision,
    change: PreferenceChange,
) -> CommandEnvelope {
    envelope(
        owner,
        Command::UpdateSettings {
            expected_revision: revision,
            default_root: None,
            max_active_downloads: Some(change),
            download_speed_limit: None,
        },
    )
}

/// Changes the most downloads at once at the current revision.
async fn set_max_active(owner: &EngineOwner, change: PreferenceChange) -> CommandResult {
    let revision = settings(owner).await.revision;
    owner
        .handle()
        .execute(max_active(owner, revision, change), ALL)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_the_download_limit_applies_at_once_and_stops_nothing() {
    let mut env = env();
    env.policy.max_active_downloads.default = Some(2);
    let paths = ["/a.bin", "/b.bin", "/c.bin", "/d.bin", "/e.bin"];
    for path in paths {
        let mut resource = Resource::new(body(8 * KIB));
        resource.stall_at = Some(4 * KIB);
        env.server.put(path, resource);
    }
    let requests = || env.server.requests.lock().unwrap().len();
    let owner = env.start().await;
    let ids = submit(&owner, &paths).await;
    wait_for(&owner, ids[1], |j| j.bytes_done > 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(requests(), 2);

    // Raising the limit starts queued downloads at once.
    set_max_active(&owner, PreferenceChange::Set(4)).await;
    wait_for(&owner, ids[3], |j| j.bytes_done > 0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(requests(), 4);
    assert_eq!(job(&owner, ids[4]).await.stage, JobStage::Queued);

    // Lowering it cancels nothing: the four keep transferring, and the
    // fifth waits until fewer than one runs.
    set_max_active(&owner, PreferenceChange::Set(1)).await;
    let limit = settings(&owner).await.max_active_downloads;
    assert_eq!((limit.value, limit.source), (1, SettingSource::Saved));
    env.server.update("/a.bin", |r| r.stall_at = None);
    complete(&owner, ids[0]).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    for id in &ids[1..4] {
        assert_eq!(job(&owner, *id).await.stage, JobStage::Transferring);
    }
    assert_eq!(job(&owner, ids[4]).await.stage, JobStage::Queued);
    assert_eq!(requests(), 4);
    for path in &paths[1..4] {
        env.server.update(path, |r| r.stall_at = None);
    }
    for id in &ids[1..4] {
        complete(&owner, *id).await;
    }
    wait_for(&owner, ids[4], |j| j.bytes_done > 0).await;
    env.server.update("/e.bin", |r| r.stall_at = None);
    complete(&owner, ids[4]).await;
    assert_eq!(env.server.most_open_bodies.load(Ordering::SeqCst), 4);
    assert_eq!(requests(), 5);
    shutdown(owner).await;

    // The saved limit survives a restart.
    let owner = env.start().await;
    let limit = settings(&owner).await.max_active_downloads;
    assert_eq!(
        (limit.value, limit.saved, limit.source),
        (1, Some(1), SettingSource::Saved)
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_policy_bounds_and_locks_the_download_limit() {
    let mut env = env();
    let owner = env.start().await;
    assert_eq!(
        settings(&owner).await.max_active_downloads,
        BoundedSetting {
            value: 4,
            source: SettingSource::BuiltIn,
            default: 4,
            saved: None,
            min: 1,
            max: 64,
            locked: false,
            conflict: false,
        }
    );
    // Every client hears of a change.
    let handle = owner.handle();
    let head = handle.hello().await.unwrap().head;
    let mut events = handle
        .subscribe(Subscribe { after: Some(head) }, ALL)
        .await
        .unwrap();
    let CommandResult::SettingsUpdated { revision, .. } =
        set_max_active(&owner, PreferenceChange::Set(12)).await
    else {
        panic!("not a settings update");
    };
    assert!(matches!(events.next().await, Some(StreamItem::Hello(_))));
    let Some(StreamItem::Event(event)) = events.next().await else {
        panic!("no event");
    };
    assert_eq!(event.kind, EventKind::SettingsChanged { revision });
    shutdown(owner).await;

    // A narrower policy keeps the saved value but runs within it.
    env.policy.max_active_downloads = IntegerPolicy {
        default: Some(3),
        min: 1,
        max: 8,
        locked: false,
    };
    let owner = env.start().await;
    let narrowed = settings(&owner).await.max_active_downloads;
    assert_eq!(
        (
            narrowed.value,
            narrowed.saved,
            narrowed.conflict,
            narrowed.source
        ),
        (8, Some(12), true, SettingSource::Saved)
    );
    let revision = settings(&owner).await.revision;
    for value in [0, 9] {
        let refused = owner
            .handle()
            .execute(
                max_active(&owner, revision, PreferenceChange::Set(value)),
                ALL,
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidRequest);
    }
    // Returning to the default lets the configured one apply.
    set_max_active(&owner, PreferenceChange::UseDefault).await;
    let reset = settings(&owner).await.max_active_downloads;
    assert_eq!(
        (reset.value, reset.saved, reset.conflict, reset.source),
        (3, None, false, SettingSource::Server)
    );
    set_max_active(&owner, PreferenceChange::Set(6)).await;
    // A change must change something.
    let empty = owner
        .handle()
        .execute(
            envelope(
                &owner,
                Command::UpdateSettings {
                    expected_revision: settings(&owner).await.revision,
                    default_root: None,
                    max_active_downloads: None,
                    download_speed_limit: None,
                },
            ),
            ALL,
        )
        .await
        .unwrap_err();
    assert_eq!(empty.code, ErrorCode::InvalidRequest);
    shutdown(owner).await;

    // A lock wins over the saved value and refuses changes.
    env.policy.max_active_downloads.default = Some(2);
    env.policy.max_active_downloads.locked = true;
    let owner = env.start().await;
    let locked = settings(&owner).await.max_active_downloads;
    assert_eq!(
        (
            locked.value,
            locked.saved,
            locked.locked,
            locked.conflict,
            locked.source
        ),
        (2, Some(6), true, true, SettingSource::Server)
    );
    let revision = settings(&owner).await.revision;
    let refused = owner
        .handle()
        .execute(max_active(&owner, revision, PreferenceChange::Set(2)), ALL)
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable);
    set_max_active(&owner, PreferenceChange::UseDefault).await;
    let cleared = settings(&owner).await.max_active_downloads;
    assert_eq!((cleared.saved, cleared.conflict), (None, false));
    shutdown(owner).await;
}

fn speed_limit(
    owner: &EngineOwner,
    revision: Revision,
    change: SpeedLimitChange,
) -> CommandEnvelope {
    envelope(
        owner,
        Command::UpdateSettings {
            expected_revision: revision,
            default_root: None,
            max_active_downloads: None,
            download_speed_limit: Some(change),
        },
    )
}

/// Changes the total download speed limit at the current revision.
async fn set_speed_limit(owner: &EngineOwner, limit: SpeedLimit) {
    let revision = settings(owner).await.revision;
    owner
        .handle()
        .execute(
            speed_limit(owner, revision, SpeedLimitChange::Set(limit)),
            ALL,
        )
        .await
        .unwrap();
}

fn rate(bytes: usize) -> SpeedLimit {
    SpeedLimit::BytesPerSecond(bytes as u64)
}

/// Small reads and checkpoints, so throttled bytes show quickly.
fn throttled_env() -> Env {
    let mut env = env();
    env.limits.read_buffer_bytes = NonZeroUsize::new(4 * KIB).unwrap();
    env.limits.checkpoint_bytes = NonZeroU64::new(16 * KIB as u64).unwrap();
    env
}

/// Most body bytes the engine may have taken in `seconds` at `limit` bytes
/// per second from `transfers` transfers: the burst, one quantum, and one
/// read buffer per transfer read ahead (`throttle` module documentation),
/// plus `debt` admitted ahead of schedule before a change.
fn allowance(limit: usize, seconds: usize, transfers: usize, debt: usize) -> usize {
    use wayfetch_engine::transfer::throttle::{burst, quantum};
    let limit = limit as u64;
    (burst(limit) + quantum(limit)) as usize + limit as usize * seconds + transfers * 4 * KIB + debt
}

#[tokio::test(flavor = "multi_thread")]
async fn downloads_share_one_speed_limit_that_changes_while_they_run() {
    let env = throttled_env();
    let data = body(256 * KIB);
    let paths = ["/a.bin", "/b.bin", "/c.bin"];
    for path in paths {
        env.server.put(path, Resource::new(data.clone()));
    }
    let owner = env.start().await;
    set_speed_limit(&owner, rate(64 * KIB)).await;
    let ids = submit(&owner, &paths).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Three downloads share 64 KiB/s instead of getting it each, and each
    // makes progress.
    let first = env.server.served_total();
    assert!(first <= allowance(64 * KIB, 1, 3, 0), "{first}");
    for path in paths {
        assert!(env.server.served(path) > 0, "{path} starved");
    }
    for &id in &ids {
        assert_eq!(job(&owner, id).await.stage, JobStage::Transferring);
    }

    // Lowering it applies to the running transfers.
    set_speed_limit(&owner, rate(16 * KIB)).await;
    let before = env.server.served_total();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let slower = env.server.served_total() - before;
    let debt = wayfetch_engine::transfer::throttle::quantum(64 * KIB as u64) as usize;
    assert!(slower <= allowance(16 * KIB, 1, 3, debt), "{slower}");

    // Removing it lets them finish at once, on the same connections.
    set_speed_limit(&owner, SpeedLimit::Unlimited).await;
    for id in ids {
        let done = complete(&owner, id).await;
        assert_eq!(done.bytes_done, data.len() as u64);
    }
    for path in paths {
        assert_eq!(env.published(path.trim_start_matches('/')), data);
    }
    assert_eq!(env.server.ranges_requested(), vec![None; 3]);
    assert_eq!(env.server.served_total(), 3 * data.len());
    shutdown(owner).await;

    // The saved Unlimited is kept across a restart, as a choice.
    let owner = env.start().await;
    let saved = settings(&owner).await.download_speed_limit;
    assert_eq!(
        (saved.value, saved.saved, saved.source),
        (
            SpeedLimit::Unlimited,
            Some(SpeedLimit::Unlimited),
            SettingSource::Saved
        )
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribers_see_live_progress_only_while_bytes_transfer() {
    let env = throttled_env();
    let data = body(160 * KIB);
    env.server.put("/live.bin", Resource::new(data.clone()));
    let owner = env.start().await;
    set_speed_limit(&owner, rate(32 * KIB)).await;
    let handle = owner.handle();
    let hello = handle.hello().await.unwrap();
    let mut stream = handle
        .subscribe(
            Subscribe {
                after: Some(hello.head),
            },
            ALL,
        )
        .await
        .unwrap();
    assert!(matches!(stream.next().await, Some(StreamItem::Hello(_))));
    let [id] = submit(&owner, &["/live.bin"]).await[..] else {
        panic!()
    };
    let mut samples = Vec::new();
    let generation = loop {
        match stream.next().await.expect("stream ended early") {
            StreamItem::Progress(sample) => samples.push(sample),
            StreamItem::Event(event) => {
                if let EventKind::JobChanged { job } = event.kind
                    && job.id == id
                    && job.stage == JobStage::Complete
                {
                    break job.generation;
                }
            }
            other => panic!("unexpected {other:?}"),
        }
    };
    // About five seconds at 32 KiB/s, one sample a second.
    assert!(samples.len() >= 3, "{samples:?}");
    let mut received = 0;
    for sample in &samples {
        assert_eq!(
            (sample.job, sample.boot_epoch, sample.generation),
            (id, hello.boot_epoch, generation)
        );
        assert_eq!(sample.bytes_total, Some(data.len() as u64));
        assert!(sample.bytes_received >= received, "{samples:?}");
        received = sample.bytes_received;
    }
    // Once past the first burst, speed follows the limit.
    let steady = &samples[samples.len() - 1];
    let limit = 32 * KIB as u64;
    assert!(
        (limit / 2..=2 * limit).contains(&steady.bytes_per_second),
        "{samples:?}"
    );
    // Samples run ahead of the 16 KiB checkpoints durable progress moves by.
    assert!(
        samples
            .iter()
            .any(|s| s.bytes_received % (16 * KIB as u64) != 0)
    );

    // Nothing more is sent once the transfer ended.
    let quiet = tokio::time::timeout(Duration::from_secs(3), stream.next()).await;
    assert!(quiet.is_err(), "{quiet:?}");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn throttling_is_not_inactivity_and_a_throttled_transfer_pauses_and_resumes() {
    let mut env = throttled_env();
    // Far shorter than the throttled transfers take.
    env.timeouts.idle_read = Duration::from_millis(300);
    let data = body(24 * KIB);
    env.server.put("/slow.bin", Resource::new(data.clone()));
    env.server.put("/paused.bin", Resource::new(data.clone()));
    let owner = env.start().await;
    set_speed_limit(&owner, rate(8 * KIB)).await;
    let ids = submit(&owner, &["/slow.bin", "/paused.bin"]).await;
    let (slow, paused) = (ids[0], ids[1]);
    // The first checkpoint lands, so the pause keeps durable bytes.
    wait_for(&owner, paused, |j| j.bytes_done >= 16 * KIB as u64).await;
    let summary = job(&owner, paused).await;
    let pause = Command::Pause {
        job: paused,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    // A pause interrupts a wait for bandwidth at once.
    let held = wait_for(&owner, paused, |j| j.stage == JobStage::Paused).await;
    assert!(held.bytes_done >= 16 * KIB as u64);
    // Waiting for bandwidth longer than the idle deadline fails nothing.
    let done = complete(&owner, slow).await;
    assert_eq!(done.bytes_done, data.len() as u64);
    assert_eq!(env.published("slow.bin"), data);

    let summary = job(&owner, paused).await;
    let resume = Command::Resume {
        job: paused,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    let done = complete(&owner, paused).await;
    assert_eq!(done.generation, summary.generation);
    assert_eq!(env.published("paused.bin"), data);
    // The resume continued from the durable bytes, not from zero.
    let ranges = env.server.ranges_requested();
    assert_eq!(ranges.last().unwrap(), &Some((held.bytes_done, None)));

    // A source that really stops still fails at the idle deadline.
    let mut stalled = Resource::new(data.clone());
    stalled.stall_at = Some(4 * KIB);
    env.server.put("/stalled.bin", stalled);
    let [id] = submit(&owner, &["/stalled.bin"]).await[..] else {
        panic!()
    };
    let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    assert_eq!(
        failed.failure.map(|f| f.code),
        Some(FailureCode::RequestTimeout)
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_operator_policy_caps_the_speed_limit_and_unlimited_cannot_bypass_it() {
    const MIB: usize = 1024 * KIB;
    let mut env = env();
    let owner = env.start().await;
    assert_eq!(
        settings(&owner).await.download_speed_limit,
        SpeedLimitSetting {
            value: SpeedLimit::Unlimited,
            source: SettingSource::BuiltIn,
            default: SpeedLimit::Unlimited,
            saved: None,
            min: 1024,
            max: SpeedLimitPolicy::MAX,
            unlimited: true,
            locked: false,
            conflict: false,
        }
    );
    set_speed_limit(&owner, SpeedLimit::Unlimited).await;
    shutdown(owner).await;

    // A maximum keeps the saved Unlimited but runs at the maximum.
    env.policy.download_speed_limit = SpeedLimitPolicy {
        default: Some(rate(MIB)),
        min: 64 * KIB as u64,
        max: Some(4 * MIB as u64),
        locked: false,
    };
    let owner = env.start().await;
    let capped = settings(&owner).await.download_speed_limit;
    assert_eq!(
        (
            capped.value,
            capped.saved,
            capped.conflict,
            capped.unlimited
        ),
        (rate(4 * MIB), Some(SpeedLimit::Unlimited), true, false)
    );
    let revision = settings(&owner).await.revision;
    for limit in [
        SpeedLimit::Unlimited,
        rate(0),
        rate(KIB),
        rate(4 * MIB + 1),
        SpeedLimit::BytesPerSecond(u64::MAX),
    ] {
        let refused = owner
            .handle()
            .execute(
                speed_limit(&owner, revision, SpeedLimitChange::Set(limit)),
                ALL,
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidRequest, "{limit:?}");
    }
    // Both preferences change together or not at all.
    let both = |max_active, speed| {
        envelope(
            &owner,
            Command::UpdateSettings {
                expected_revision: revision,
                default_root: None,
                max_active_downloads: Some(PreferenceChange::Set(max_active)),
                download_speed_limit: Some(SpeedLimitChange::Set(speed)),
            },
        )
    };
    let refused = owner
        .handle()
        .execute(both(2, SpeedLimit::Unlimited), ALL)
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidRequest);
    assert_eq!(settings(&owner).await.max_active_downloads.saved, None);
    let accepted = both(2, rate(2 * MIB));
    let first = owner.handle().execute(accepted.clone(), ALL).await.unwrap();
    // A retry replays the result instead of changing anything again.
    assert_eq!(owner.handle().execute(accepted, ALL).await.unwrap(), first);
    let saved = settings(&owner).await;
    assert_eq!(saved.revision, revision.next().unwrap());
    assert_eq!(saved.max_active_downloads.value, 2);
    assert_eq!(
        (
            saved.download_speed_limit.value,
            saved.download_speed_limit.conflict
        ),
        (rate(2 * MIB), false)
    );
    // A stale revision is refused; reading-only callers may not change it.
    let stale = owner
        .handle()
        .execute(
            speed_limit(&owner, revision, SpeedLimitChange::Set(rate(MIB))),
            ALL,
        )
        .await
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::RevisionConflict);
    let denied = owner
        .handle()
        .execute(
            speed_limit(&owner, saved.revision, SpeedLimitChange::Set(rate(MIB))),
            &[Permission::Read, Permission::Submit],
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    // Returning to the default lets the configured one apply.
    owner
        .handle()
        .execute(
            speed_limit(&owner, saved.revision, SpeedLimitChange::UseDefault),
            ALL,
        )
        .await
        .unwrap();
    let reset = settings(&owner).await.download_speed_limit;
    assert_eq!(
        (reset.value, reset.saved, reset.source),
        (rate(MIB), None, SettingSource::Server)
    );
    set_speed_limit(&owner, rate(3 * MIB)).await;
    shutdown(owner).await;

    // A lock wins over the saved limit and refuses changes.
    env.policy.download_speed_limit.locked = true;
    let owner = env.start().await;
    let locked = settings(&owner).await.download_speed_limit;
    assert_eq!(
        (locked.value, locked.saved, locked.locked, locked.conflict),
        (rate(MIB), Some(rate(3 * MIB)), true, true)
    );
    let revision = settings(&owner).await.revision;
    let refused = owner
        .handle()
        .execute(
            speed_limit(&owner, revision, SpeedLimitChange::Set(rate(MIB))),
            ALL,
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribers_replay_within_bounds_and_resync_beyond_them() {
    let mut env = env();
    env.limits.event_buffer = NonZeroUsize::new(16).unwrap();
    env.server.put("/one.bin", Resource::new(body(KIB)));
    let owner = env.start().await;
    let handle = owner.handle();
    let head = handle.hello().await.unwrap().head;

    // A live subscriber sees the job through to completion.
    let mut live = handle
        .subscribe(Subscribe { after: Some(head) }, ALL)
        .await
        .unwrap();
    assert!(matches!(live.next().await, Some(StreamItem::Hello(_))));
    let [id] = submit(&owner, &["/one.bin"]).await[..] else {
        panic!()
    };
    let mut last = head.position;
    loop {
        let event = match live.next().await {
            Some(StreamItem::Event(event)) => event,
            Some(StreamItem::Progress(_)) => continue,
            other => panic!("stream ended early: {other:?}"),
        };
        assert_eq!(event.cursor.position, last + 1, "in order, no gaps");
        last = event.cursor.position;
        if let EventKind::JobChanged { job } = event.kind
            && job.id == id
            && job.stage == JobStage::Complete
        {
            break;
        }
    }

    // A subscriber that stops reading while more events than the ring holds
    // are committed is told to resynchronize instead of being buffered for.
    let mark = handle.hello().await.unwrap().head;
    let mut slow = handle
        .subscribe(Subscribe { after: None }, ALL)
        .await
        .unwrap();
    let paths: Vec<String> = (0..10).map(|i| format!("/q{i}.missing")).collect();
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    for id in submit(&owner, &paths).await {
        wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    }
    assert!(matches!(slow.next().await, Some(StreamItem::Hello(_))));
    let mut items = Vec::new();
    while let Some(item) = slow.next().await {
        items.push(item);
    }
    assert_eq!(
        items.last(),
        Some(&StreamItem::Resync {
            reason: ResyncReason::Lagged
        })
    );
    assert!(items.len() <= 1 + env.limits.event_buffer.get());

    // Replaying more than the bound resynchronizes at once.
    let mut behind = handle
        .subscribe(Subscribe { after: Some(mark) }, ALL)
        .await
        .unwrap();
    assert!(matches!(behind.next().await, Some(StreamItem::Hello(_))));
    assert_eq!(
        behind.next().await,
        Some(StreamItem::Resync {
            reason: ResyncReason::Lagged
        })
    );
    assert_eq!(behind.next().await, None);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pruned_events_and_old_instances_resync_after_restart() {
    let mut env = env();
    env.limits.event_retention = NonZeroU64::new(2).unwrap();
    env.server.put("/one.bin", Resource::new(body(KIB)));
    let owner = env.start().await;
    let first = owner.handle().hello().await.unwrap().head;
    let [id] = submit(&owner, &["/one.bin"]).await[..] else {
        panic!()
    };
    complete(&owner, id).await;
    shutdown(owner).await;

    // Startup pruned all but the newest two events.
    let owner = env.start().await;
    let handle = owner.handle();
    let mut expired = handle
        .subscribe(Subscribe { after: Some(first) }, ALL)
        .await
        .unwrap();
    assert!(matches!(expired.next().await, Some(StreamItem::Hello(_))));
    assert_eq!(
        expired.next().await,
        Some(StreamItem::Resync {
            reason: ResyncReason::CursorExpired
        })
    );
    let head = handle.hello().await.unwrap().head;
    let foreign = wayfetch_protocol::EventCursor {
        instance: wayfetch_protocol::InstanceId::generate(),
        ..head
    };
    let mut other = handle
        .subscribe(
            Subscribe {
                after: Some(foreign),
            },
            ALL,
        )
        .await
        .unwrap();
    other.next().await;
    assert_eq!(
        other.next().await,
        Some(StreamItem::Resync {
            reason: ResyncReason::UnknownInstance
        })
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_checkpoint_fails_the_job_and_frees_its_slot() {
    let mut env = env();
    env.policy.max_active_downloads.default = Some(1);
    let mut first = Resource::new(body(32 * KIB));
    first.stall_at = Some(8 * KIB);
    env.server.put("/first.bin", first);
    env.server.put("/second.bin", Resource::new(body(8 * KIB)));
    let owner = env.start().await;
    let ids = submit(&owner, &["/first.bin", "/second.bin"]).await;
    wait_for(&owner, ids[0], |j| j.bytes_done == 8 * KIB as u64).await;
    // Something outside the transfer removes its staging record, so the
    // next checkpoint is refused.
    let db = rusqlite::Connection::open(env.state.join("state.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute("DELETE FROM staging_files", []).unwrap();
    drop(db);
    env.server.update("/first.bin", |r| r.stall_at = None);

    let failed = wait_for(&owner, ids[0], |j| j.stage == JobStage::Failed).await;
    assert_eq!(failed.failure.unwrap().code, FailureCode::IntegrityMismatch);
    // The response and the slot were released: the next job runs.
    complete(&owner, ids[1]).await;
    assert_eq!(env.server.open_bodies.load(Ordering::SeqCst), 0);
    // Failing is final: the job was not retried.
    assert_eq!(env.server.requests.lock().unwrap().len(), 2);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unusable_response_metadata_never_loops() {
    let env = env();
    let cases = [
        // Longer than a stored suggestion may be.
        (
            "/long-name.bin",
            Some(format!("{}.bin", "n".repeat(1025))),
            None,
        ),
        // Longer than one path component may be.
        (
            "/component.bin",
            Some(format!("{}.bin", "c".repeat(300))),
            None,
        ),
        // Longer than a stored entity tag may be.
        (
            "/long-etag.bin",
            None,
            Some(format!("\"{}\"", "e".repeat(2000))),
        ),
    ];
    for (path, filename, etag) in &cases {
        let mut resource = Resource::new(body(4 * KIB));
        resource.filename = filename.clone();
        if etag.is_some() {
            resource.etag = etag.clone();
        }
        env.server.put(path, resource);
    }
    let owner = env.start().await;
    let paths: Vec<&str> = cases.iter().map(|c| c.0).collect();
    let ids = submit(&owner, &paths).await;
    for (id, path) in ids.into_iter().zip(&paths) {
        let done = complete(&owner, id).await;
        // The URL names the file instead.
        assert_eq!(done.artifact.unwrap().path, &path[1..]);
    }
    // One request and one staging file each.
    assert_eq!(env.server.requests.lock().unwrap().len(), cases.len());
    assert_eq!(env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_pause_does_not_interrupt_a_resumed_transfer() {
    let env = env();
    let data = body(32 * KIB);
    let mut resource = Resource::new(data.clone());
    resource.stall_at = Some(8 * KIB);
    env.server.put("/job.bin", resource);
    let owner = env.start().await;
    let handle = owner.handle();
    let [id] = submit(&owner, &["/job.bin"]).await[..] else {
        panic!()
    };
    wait_for(&owner, id, |j| j.bytes_done == 8 * KIB as u64).await;
    let summary = job(&owner, id).await;
    let pause = envelope(
        &owner,
        Command::Pause {
            job: id,
            expected_revision: summary.revision,
        },
    );
    let paused = handle.execute(pause.clone(), ALL).await.unwrap();
    let summary = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    let resume = Command::Resume {
        job: id,
        expected_revision: summary.revision,
    };
    handle.execute(envelope(&owner, resume), ALL).await.unwrap();
    let resumed = wait_for(&owner, id, |j| j.stage == JobStage::Transferring).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    // A client retrying its old pause gets the stored result and changes
    // nothing.
    assert_eq!(handle.execute(pause, ALL).await.unwrap(), paused);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let now = job(&owner, id).await;
    assert_eq!(
        (now.stage, now.generation, now.revision),
        (JobStage::Transferring, resumed.generation, resumed.revision)
    );
    env.server.update("/job.bin", |r| r.stall_at = None);
    let done = complete(&owner, id).await;
    assert_eq!(done.generation, resumed.generation);
    assert_eq!(env.published("job.bin"), data);
    // The first request and the resume; nothing restarted.
    assert_eq!(env.server.requests.lock().unwrap().len(), 2);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_storage_failure_during_recovery_waits_for_the_next_start() {
    let env = env();
    let data = body(32 * KIB);
    let id = interrupt_at(&env, "/held.bin", &data, 8 * KIB).await;
    // Adopting the partial fails in the database, as a full disk would.
    let db = rusqlite::Connection::open(env.state.join("state.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse BEFORE UPDATE ON staging_files \
         BEGIN SELECT RAISE(ABORT, 'disk full'); END",
    )
    .unwrap();

    let owner = env.start().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // The fault clears, but the job is not retried in this run: a retry
    // loop would have picked it up again by now.
    db.execute_batch("DROP TRIGGER refuse").unwrap();
    drop(db);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let summary = job(&owner, id).await;
    assert_eq!(summary.stage, JobStage::Recovering);
    assert_eq!(env.server.requests.lock().unwrap().len(), 1);
    // A held job can still be paused.
    let pause = Command::Pause {
        job: id,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    shutdown(owner).await;

    // After the next start it resumes and completes.
    let owner = env.start().await;
    let summary = job(&owner, id).await;
    let resume = Command::Resume {
        job: id,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    complete(&owner, id).await;
    assert_eq!(env.published("held.bin"), data);
    shutdown(owner).await;
}

// Destinations (ADR-17, ADR-29): the default root for new jobs, and roots
// a host registers on its user's request.

async fn settings(owner: &EngineOwner) -> Settings {
    match owner.handle().query(Query::Settings {}, ALL).await.unwrap() {
        QueryResult::Settings(settings) => settings,
        other => panic!("{other:?}"),
    }
}

fn choose(owner: &EngineOwner, revision: Revision, root: RootId) -> CommandEnvelope {
    envelope(
        owner,
        Command::UpdateSettings {
            expected_revision: revision,
            default_root: Some(root),
            max_active_downloads: None,
            download_speed_limit: None,
        },
    )
}

/// A directory beside the environment's download root.
fn folder(env: &Env, name: &str) -> PathBuf {
    let path = env.root.parent().unwrap().join(name);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_default_applies_only_to_later_jobs_and_survives_restart() {
    let mut env = env();
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    assert_ne!(media_id, ROOT);
    env.server.put("/a.bin", Resource::new(body(5 * KIB)));
    env.server.put("/b.bin", Resource::new(body(6 * KIB)));
    let owner = env.start().await;
    let [a] = submit(&owner, &["/a.bin"]).await[..] else {
        panic!()
    };
    assert_eq!(complete(&owner, a).await.destination, Some(ROOT));

    let before = settings(&owner).await;
    assert_eq!(before.default_root, Some(ROOT));
    let listed: Vec<_> = before.roots.iter().map(|r| (r.id, r.available)).collect();
    assert_eq!(listed, [(ROOT, true), (media_id, true)]);
    assert_eq!(before.roots[1].path, media.to_str().unwrap());

    let change = choose(&owner, before.revision, media_id);
    let updated = owner.handle().execute(change.clone(), ALL).await.unwrap();
    let revision = before.revision.next().unwrap();
    assert_eq!(
        updated,
        CommandResult::SettingsUpdated {
            revision,
            changed: true
        }
    );
    // A retry replays the result; a stale revision conflicts.
    assert_eq!(owner.handle().execute(change, ALL).await.unwrap(), updated);
    let stale = owner
        .handle()
        .execute(choose(&owner, before.revision, ROOT), ALL)
        .await
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::RevisionConflict);
    assert_eq!(stale.current_revision, Some(revision));

    let [b] = submit(&owner, &["/b.bin"]).await[..] else {
        panic!()
    };
    let done = complete(&owner, b).await;
    assert_eq!(done.destination, Some(media_id));
    assert_eq!(done.artifact.unwrap().root, media_id);
    assert_eq!(std::fs::read(media.join("b.bin")).unwrap(), body(6 * KIB));
    assert!(!env.root.join("b.bin").exists());
    // Read permission is enough to see the settings, not to change them.
    let denied = owner
        .handle()
        .execute(choose(&owner, revision, ROOT), &[Permission::Read])
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    shutdown(owner).await;

    let owner = env.start().await;
    let after = settings(&owner).await;
    assert_eq!(
        (after.revision, after.default_root),
        (revision, Some(media_id))
    );
    assert_eq!(job(&owner, a).await.destination, Some(ROOT));
    assert_eq!(job(&owner, b).await.destination, Some(media_id));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_and_unavailable_roots_cannot_become_the_default() {
    let mut env = env();
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    let owner = env.start().await;
    let before = settings(&owner).await;

    let unknown = RootId::from_u64(99).unwrap();
    let err = owner
        .handle()
        .execute(choose(&owner, before.revision, unknown), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    // The drive is unmounted: its mount point is an empty directory.
    let mounted = folder(&env, "media-mounted");
    std::fs::remove_dir(&mounted).unwrap();
    std::fs::rename(&media, &mounted).unwrap();
    std::fs::create_dir(&media).unwrap();
    let missing = settings(&owner).await;
    let shown = missing.roots.iter().find(|r| r.id == media_id).unwrap();
    assert!(!shown.available);
    assert!(
        shown
            .problem
            .as_ref()
            .unwrap()
            .as_str()
            .contains("not mounted"),
        "{shown:?}"
    );
    let err = owner
        .handle()
        .execute(choose(&owner, before.revision, media_id), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    // Nothing changed, and the empty mount point was not written to.
    let after = settings(&owner).await;
    assert_eq!(
        (after.revision, after.default_root),
        (before.revision, Some(ROOT))
    );
    assert_eq!(std::fs::read_dir(&media).unwrap().count(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_keeps_its_root_and_blocks_while_it_is_not_permitted() {
    let mut env = env();
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    let data = body(20 * KIB);
    let mut resource = Resource::new(data.clone());
    resource.stall_at = Some(8 * KIB);
    env.server.put("/kept.bin", resource);
    let owner = env.start().await;
    let revision = settings(&owner).await.revision;
    owner
        .handle()
        .execute(choose(&owner, revision, media_id), ALL)
        .await
        .unwrap();
    let [id] = submit(&owner, &["/kept.bin"]).await[..] else {
        panic!()
    };
    wait_for(&owner, id, |j| j.bytes_done == 8 * KIB as u64).await;
    shutdown(owner).await;
    env.server.update("/kept.bin", |r| r.stall_at = None);

    // The operator stops configuring the root: the job is not redirected
    // to the download root, and its partial stays where it is.
    env.roots.clear();
    let owner = env.start().await;
    let blocked = wait_for(&owner, id, |j| j.stage == JobStage::Blocked).await;
    assert_eq!(
        blocked.failure.unwrap().code,
        FailureCode::DestinationUnavailable
    );
    assert_eq!(blocked.destination, Some(media_id));
    assert_eq!(support::staging_files(&media), 1);
    let listed = settings(&owner).await;
    assert_eq!(listed.default_root, Some(ROOT));
    assert!(listed.roots.iter().all(|r| r.id != media_id));
    shutdown(owner).await;

    // Configured again, the job continues there once resumed.
    env.roots.push(media.clone());
    let owner = env.start().await;
    let blocked = job(&owner, id).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: blocked.revision,
    };
    let paused = owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let CommandResult::JobUpdated { revision, .. } = paused else {
        panic!("{paused:?}")
    };
    let resume = Command::Resume {
        job: id,
        expected_revision: revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    complete(&owner, id).await;
    assert_eq!(std::fs::read(media.join("kept.bin")).unwrap(), data);
    assert!(!env.root.join("kept.bin").exists());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_lists_server_folders_to_choose_one_to_register() {
    let env = env();
    let owner = env.start().await;
    let handle = owner.handle();
    let base = folder(&env, "browse");
    for name in ["beta", "Alpha", ".hidden", "gamma"] {
        std::fs::create_dir(base.join(name)).unwrap();
    }
    std::fs::write(base.join("file.txt"), b"not a folder").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(base.join("gamma"), base.join("linked")).unwrap();

    // Only folders, without hidden ones, in a case-insensitive order; a
    // path given with `..` is resolved.
    let listing = handle
        .browse_folders(Some(base.join("beta").join("..")), ALL)
        .await
        .unwrap();
    let base_text = std::fs::canonicalize(&base).unwrap();
    assert_eq!(listing.path, base_text.to_str().unwrap());
    assert_eq!(
        listing.parent.as_deref(),
        base_text.parent().unwrap().to_str()
    );
    let expected: &[&str] = if cfg!(unix) {
        &["Alpha", "beta", "gamma", "linked"]
    } else {
        &["Alpha", "beta", "gamma"]
    };
    assert_eq!(listing.folders, expected);
    assert!(!listing.truncated);
    // The top of the filesystem when no folder is named.
    let top = handle.browse_folders(None, ALL).await.unwrap();
    assert_eq!(top.parent, None);

    for (path, code) in [
        (base.join("missing"), ErrorCode::NotFound),
        (base.join("file.txt"), ErrorCode::NotApplicable),
        (PathBuf::from("relative"), ErrorCode::InvalidRequest),
    ] {
        let err = handle.browse_folders(Some(path), ALL).await.unwrap_err();
        assert_eq!(err.code, code, "{err}");
    }
    let err = handle
        .browse_folders(Some(base.clone()), &[Permission::Read, Permission::Submit])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);

    // The state directory, or a folder inside it, cannot be a destination.
    for refused in [env.state.clone(), env.state.join("..").join("state")] {
        let err = handle.register_root(refused, ALL).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest, "{err}");
    }
    let registered = handle.register_root(base.join("Alpha"), ALL).await.unwrap();
    assert!(registered.available);
    shutdown(owner).await;
}

fn remove_root(owner: &EngineOwner, revision: Revision, root: RootId) -> CommandEnvelope {
    envelope(
        owner,
        Command::RemoveRoot {
            expected_revision: revision,
            root,
        },
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn only_an_idle_folder_that_is_not_the_default_can_be_removed() {
    let env = env();
    let mut resource = Resource::new(body(16 * KIB));
    resource.stall_at = Some(8 * KIB);
    env.server.put("/slow.bin", resource);
    let owner = env.start().await;
    let handle = owner.handle();
    let chosen = folder(&env, "chosen");
    let added = handle.register_root(chosen.clone(), ALL).await.unwrap();
    assert!(added.added);
    let listed = settings(&owner).await;
    assert!(listed.roots.iter().any(|r| r.id == ROOT && !r.added));
    let code = |result: Result<CommandResult, ProtocolError>| result.unwrap_err().code;

    // The default, here the operator's folder, a stale revision, and the
    // default chosen are refused.
    let revision = listed.revision;
    let refused = handle
        .execute(remove_root(&owner, revision, ROOT), ALL)
        .await;
    assert_eq!(code(refused), ErrorCode::NotApplicable);
    let stale = Revision::INITIAL
        .next()
        .unwrap()
        .next()
        .unwrap()
        .next()
        .unwrap();
    let refused = handle
        .execute(remove_root(&owner, stale, added.id), ALL)
        .await;
    assert_eq!(code(refused), ErrorCode::RevisionConflict);
    handle
        .execute(choose(&owner, revision, added.id), ALL)
        .await
        .unwrap();
    let revision = settings(&owner).await.revision;
    let refused = handle
        .execute(remove_root(&owner, revision, added.id), ALL)
        .await;
    assert_eq!(code(refused), ErrorCode::NotApplicable);

    // A download still saving there keeps it.
    let [job] = submit(&owner, &["/slow.bin"]).await[..] else {
        panic!()
    };
    wait_for(&owner, job, |j| j.bytes_done > 0).await;
    handle
        .execute(choose(&owner, revision, ROOT), ALL)
        .await
        .unwrap();
    let revision = settings(&owner).await.revision;
    let refused = handle
        .execute(remove_root(&owner, revision, added.id), ALL)
        .await;
    assert_eq!(code(refused), ErrorCode::NotApplicable);
    env.server.update("/slow.bin", |r| r.stall_at = None);
    complete(&owner, job).await;

    // A link collected to be saved there does not keep it.
    handle
        .execute(
            collect(&owner, &["https://files.example/later.bin"], Some(added.id)),
            ALL,
        )
        .await
        .unwrap();

    // Then it goes, its files stay, and a retry replays.
    let removal = remove_root(&owner, revision, added.id);
    let removed = handle.execute(removal.clone(), ALL).await.unwrap();
    assert!(matches!(
        removed,
        CommandResult::SettingsUpdated { changed: true, .. }
    ));
    assert_eq!(handle.execute(removal.clone(), ALL).await.unwrap(), removed);
    assert!(
        !settings(&owner)
            .await
            .roots
            .iter()
            .any(|r| r.id == added.id)
    );
    assert!(chosen.join("slow.bin").exists());
    // The link stays, and starting it is refused until it is given
    // another folder.
    let page = links(&owner).await;
    let [link] = &page.items[..] else { panic!() };
    assert_eq!(link.destination, Some(added.id));
    assert_eq!(link.destination_path.as_deref(), chosen.to_str());
    let (started, refused) = applied(&owner, start_links(&owner, &[link])).await;
    assert!(started.is_empty());
    assert_eq!(refused.len(), 1);
    assert_eq!(links(&owner).await.total, 1);
    let refused = handle
        .execute(
            remove_root(&owner, settings(&owner).await.revision, added.id),
            ALL,
        )
        .await;
    assert_eq!(code(refused), ErrorCode::NotFound);

    // Adding the folder again brings back the same root, and an old
    // removal replayed afterwards does not hide it.
    let again = handle.register_root(chosen.clone(), ALL).await.unwrap();
    assert_eq!(again.id, added.id);
    assert_eq!(handle.execute(removal, ALL).await.unwrap(), removed);
    assert!(
        settings(&owner)
            .await
            .roots
            .iter()
            .any(|r| r.id == added.id)
    );
    shutdown(owner).await;

    // A removal lasts across restarts.
    let owner = env.start().await;
    let revision = settings(&owner).await.revision;
    owner
        .handle()
        .execute(remove_root(&owner, revision, added.id), ALL)
        .await
        .unwrap();
    shutdown(owner).await;
    let owner = env.start().await;
    assert!(
        !settings(&owner)
            .await
            .roots
            .iter()
            .any(|r| r.id == added.id)
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_folder_stays_removed_until_added_again() {
    let mut env = env();
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    env.server.put("/a.bin", Resource::new(body(5 * KIB)));
    let owner = env.start().await;
    let handle = owner.handle();
    let listed = settings(&owner).await;
    assert_eq!(listed.default_root, Some(ROOT));
    assert!(listed.roots.iter().all(|r| !r.added));

    // The operator's download root is the default until another is made
    // the default; then it can be removed.
    let refused = handle
        .execute(remove_root(&owner, listed.revision, ROOT), ALL)
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable);
    handle
        .execute(choose(&owner, listed.revision, media_id), ALL)
        .await
        .unwrap();
    let revision = settings(&owner).await.revision;
    handle
        .execute(remove_root(&owner, revision, ROOT), ALL)
        .await
        .unwrap();
    let after = settings(&owner).await;
    assert_eq!(after.default_root, Some(media_id));
    assert!(!after.roots.iter().any(|r| r.id == ROOT));
    // The configured folder it was is still a root, so it cannot be
    // removed as the default now.
    let refused = handle
        .execute(remove_root(&owner, after.revision, media_id), ALL)
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable);
    let [a] = submit(&owner, &["/a.bin"]).await[..] else {
        panic!()
    };
    assert_eq!(complete(&owner, a).await.destination, Some(media_id));
    shutdown(owner).await;

    // Still configured, yet it stays removed across restarts.
    let owner = env.start().await;
    let after = settings(&owner).await;
    assert_eq!(after.default_root, Some(media_id));
    assert!(!after.roots.iter().any(|r| r.id == ROOT));
    // Adding the folder again restores the same root.
    let again = owner
        .handle()
        .register_root(env.root.clone(), ALL)
        .await
        .unwrap();
    assert_eq!(again.id, ROOT);
    assert!(settings(&owner).await.roots.iter().any(|r| r.id == ROOT));
    shutdown(owner).await;
    let owner = env.start().await;
    assert!(settings(&owner).await.roots.iter().any(|r| r.id == ROOT));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registered_root_stays_permitted_across_restarts() {
    let env = env();
    let owner = env.start().await;
    let handle = owner.handle();
    let missing = env.root.parent().unwrap().join("missing");
    let err = handle
        .register_root(missing.clone(), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    assert!(!missing.exists());
    let err = handle
        .register_root("relative".into(), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    let err = handle
        .register_root(folder(&env, "x"), &[Permission::Read])
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    assert_eq!(settings(&owner).await.roots.len(), 1);

    let chosen = folder(&env, "chosen");
    let registered = handle.register_root(chosen.clone(), ALL).await.unwrap();
    assert!(registered.available);
    assert_eq!(registered.path, chosen.to_str().unwrap());
    assert_eq!(
        handle.register_root(chosen.clone(), ALL).await.unwrap().id,
        registered.id
    );
    shutdown(owner).await;

    // Not configured, yet still a choice after a restart.
    let owner = env.start().await;
    let listed = settings(&owner).await;
    assert!(
        listed
            .roots
            .iter()
            .any(|r| r.id == registered.id && r.available)
    );
    shutdown(owner).await;
}

// Changes to finished jobs (ADR-16, ADR-23): renaming the published file,
// removing a job from lists, and reading the link it was submitted with.

fn rename(owner: &EngineOwner, job: &JobSummary, name: &str) -> CommandEnvelope {
    envelope(
        owner,
        Command::Rename {
            job: job.id,
            expected_revision: job.revision,
            name: name.into(),
        },
    )
}

fn remove(owner: &EngineOwner, job: &JobSummary) -> CommandEnvelope {
    envelope(
        owner,
        Command::Remove {
            job: job.id,
            expected_revision: job.revision,
        },
    )
}

async fn refused(owner: &EngineOwner, envelope: CommandEnvelope) -> ErrorCode {
    owner
        .handle()
        .execute(envelope, ALL)
        .await
        .unwrap_err()
        .code
}

async fn counted(owner: &EngineOwner) -> u64 {
    let query = Query::CountJobs {
        filter: JobFilter::default(),
    };
    match owner.handle().query(query, ALL).await.unwrap() {
        QueryResult::JobCounts(counts) => counts.stages.values().sum(),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_published_file_is_renamed_in_place_without_replacing_anything() {
    let env = env();
    for path in ["/a.bin", "/c.bin"] {
        env.server.put(path, Resource::new(body(3 * KIB)));
    }
    let owner = env.start().await;
    let [a, c] = submit(&owner, &["/a.bin", "/c.bin"]).await[..] else {
        panic!()
    };
    let done = complete(&owner, a).await;
    complete(&owner, c).await;
    std::fs::write(env.root.join("taken.bin"), b"user data").unwrap();

    // Taken names, other folders, and names the policy refuses are
    // refused, and nothing is touched.
    assert_eq!(
        refused(&owner, rename(&owner, &done, "taken.bin")).await,
        ErrorCode::AlreadyExists
    );
    assert_eq!(
        std::fs::read(env.root.join("taken.bin")).unwrap(),
        b"user data"
    );
    for bad in ["", "../a.bin", "sub/a.bin", "a\\b", "what?", ".wayfetch"] {
        assert_eq!(
            refused(&owner, rename(&owner, &done, bad)).await,
            ErrorCode::InvalidRequest,
            "{bad:?}"
        );
    }
    // Another job's file holds the name, whatever its case.
    assert_eq!(
        refused(&owner, rename(&owner, &done, "C.BIN")).await,
        ErrorCode::AlreadyExists
    );
    // Changing only the case is refused: on a case-insensitive
    // filesystem both names are one file.
    assert_eq!(
        refused(&owner, rename(&owner, &done, "A.BIN")).await,
        ErrorCode::InvalidRequest
    );
    assert_eq!(env.published("a.bin"), body(3 * KIB));
    assert_eq!(job(&owner, a).await, done);

    let change = rename(&owner, &done, "b.bin");
    let renamed = owner.handle().execute(change.clone(), ALL).await.unwrap();
    let revision = done.revision.next().unwrap();
    assert_eq!(
        renamed,
        CommandResult::JobUpdated {
            job: a,
            revision,
            changed: true
        }
    );
    assert!(!env.root.join("a.bin").exists());
    assert_eq!(env.published("b.bin"), body(3 * KIB));
    let after = job(&owner, a).await;
    assert_eq!(after.revision, revision);
    assert_eq!(after.artifact.as_ref().unwrap().path, "b.bin");
    // A retry replays; the old revision conflicts.
    assert_eq!(owner.handle().execute(change, ALL).await.unwrap(), renamed);
    assert_eq!(
        refused(&owner, rename(&owner, &done, "d.bin")).await,
        ErrorCode::RevisionConflict
    );
    // The same name changes nothing.
    let same = owner
        .handle()
        .execute(rename(&owner, &after, "b.bin"), ALL)
        .await
        .unwrap();
    assert!(matches!(
        same,
        CommandResult::JobUpdated { changed: false, .. }
    ));
    // The host can open the file at its new name.
    assert_eq!(
        owner.handle().published_path(a, ALL).await.unwrap(),
        env.root.join("b.bin")
    );

    // The old name is free for a new download.
    let [again] = submit(&owner, &["/a.bin"]).await[..] else {
        panic!()
    };
    assert_eq!(
        complete(&owner, again).await.artifact.unwrap().path,
        "a.bin"
    );

    // A file replaced outside Wayfetch is neither renamed nor opened.
    std::fs::remove_file(env.root.join("b.bin")).unwrap();
    std::fs::write(env.root.join("b.bin"), b"replaced").unwrap();
    assert_eq!(
        refused(&owner, rename(&owner, &after, "e.bin")).await,
        ErrorCode::NotApplicable
    );
    assert!(!env.root.join("e.bin").exists());
    let open = owner.handle().published_path(a, ALL).await.unwrap_err();
    assert_eq!(open.code, ErrorCode::NotApplicable);
    shutdown(owner).await;

    // The name survives a restart.
    let owner = env.start().await;
    let restarted = job(&owner, a).await;
    assert_eq!(restarted.artifact.unwrap().path, "b.bin");
    assert_eq!(restarted.revision, revision);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_completed_jobs_are_renamed_and_opened() {
    let env = env();
    let mut stalled = Resource::new(body(32 * KIB));
    stalled.stall_at = Some(4 * KIB);
    env.server.put("/s.bin", stalled);
    let owner = env.start().await;
    let [s] = submit(&owner, &["/s.bin"]).await[..] else {
        panic!()
    };
    let running = wait_for(&owner, s, |j| j.bytes_done > 0).await;
    assert_eq!(
        refused(&owner, rename(&owner, &running, "x.bin")).await,
        ErrorCode::NotApplicable
    );
    let open = owner.handle().published_path(s, ALL).await.unwrap_err();
    assert_eq!(open.code, ErrorCode::NotApplicable);
    assert_eq!(
        refused(&owner, remove(&owner, &running)).await,
        ErrorCode::NotApplicable
    );
    // Renaming needs control.
    let denied = owner
        .handle()
        .execute(rename(&owner, &running, "x.bin"), &[Permission::Read])
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_jobs_leave_lists_and_counts_but_keep_their_files() {
    let env = env();
    env.server.put("/done.bin", Resource::new(body(3 * KIB)));
    let mut stalled = Resource::new(body(32 * KIB));
    stalled.stall_at = Some(8 * KIB);
    env.server.put("/cancelled.bin", stalled);
    let owner = env.start().await;
    let ids = submit(&owner, &["/done.bin", "/cancelled.bin", "/missing.bin"]).await;
    let [done, cancelled, failed] = ids[..] else {
        panic!()
    };
    let finished = complete(&owner, done).await;
    let partial = wait_for(&owner, cancelled, |j| j.bytes_done == 8 * KIB as u64).await;
    let cancel = Command::Cancel {
        job: cancelled,
        expected_revision: partial.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, cancel), ALL)
        .await
        .unwrap();
    let cancelled_job = wait_for(&owner, cancelled, |j| j.stage == JobStage::Cancelled).await;
    let failed_job = wait_for(&owner, failed, |j| j.stage == JobStage::Failed).await;
    assert_eq!(counted(&owner).await, 3);

    // The submitted link is readable with its own permission only.
    let source = owner
        .handle()
        .query(Query::JobSource { job: done }, ALL)
        .await
        .unwrap();
    assert_eq!(
        source,
        QueryResult::JobSource {
            job: done,
            url: "https://files.example/done.bin".into()
        }
    );
    let denied = owner
        .handle()
        .query(Query::JobSource { job: done }, &[Permission::Read])
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);

    let head = owner.handle().hello().await.unwrap().head;
    let mut events = owner
        .handle()
        .subscribe(Subscribe { after: Some(head) }, ALL)
        .await
        .unwrap();
    let removal = remove(&owner, &finished);
    let removed = owner.handle().execute(removal.clone(), ALL).await.unwrap();
    assert_eq!(
        removed,
        CommandResult::JobUpdated {
            job: done,
            revision: finished.revision,
            changed: true
        }
    );
    for summary in [&cancelled_job, &failed_job] {
        owner
            .handle()
            .execute(remove(&owner, summary), ALL)
            .await
            .unwrap();
    }
    let mut gone = Vec::new();
    while gone.len() < 3 {
        if let Some(StreamItem::Event(event)) = events.next().await
            && let EventKind::JobRemoved { job } = event.kind
        {
            gone.push(job);
        }
    }
    assert_eq!(gone, [done, cancelled, failed]);

    // Gone from lists, counts, and every request, but not from disk.
    assert!(jobs(&owner).await.is_empty());
    assert_eq!(counted(&owner).await, 0);
    for query in [
        Query::InspectJob { job: done },
        Query::JobSource { job: done },
    ] {
        let err = owner.handle().query(query, ALL).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }
    assert_eq!(owner.handle().execute(removal, ALL).await.unwrap(), removed);
    assert_eq!(
        refused(&owner, remove(&owner, &finished)).await,
        ErrorCode::NotFound
    );
    assert_eq!(
        refused(&owner, rename(&owner, &finished, "x.bin")).await,
        ErrorCode::NotFound
    );
    assert_eq!(
        owner
            .handle()
            .published_path(done, ALL)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotFound
    );
    assert_eq!(env.published("done.bin"), body(3 * KIB));
    assert_eq!(env.staging_files(), 1, "the cancelled partial stays");
    shutdown(owner).await;

    let owner = env.start().await;
    assert!(jobs(&owner).await.is_empty());
    assert_eq!(counted(&owner).await, 0);
    assert_eq!(env.staging_files(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn blocked_jobs_do_not_hold_back_the_jobs_behind_them() {
    let mut env = env();
    env.policy.max_active_downloads.default = Some(2);
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    for name in ["/a.bin", "/b.bin"] {
        let mut resource = Resource::new(body(20 * KIB));
        resource.stall_at = Some(8 * KIB);
        env.server.put(name, resource);
    }
    env.server.put("/c.bin", Resource::new(body(5 * KIB)));
    let owner = env.start().await;
    let revision = settings(&owner).await.revision;
    owner
        .handle()
        .execute(choose(&owner, revision, media_id), ALL)
        .await
        .unwrap();
    let held = submit(&owner, &["/a.bin", "/b.bin"]).await;
    for id in &held {
        wait_for(&owner, *id, |j| j.bytes_done == 8 * KIB as u64).await;
    }
    owner
        .handle()
        .execute(choose(&owner, revision.next().unwrap(), ROOT), ALL)
        .await
        .unwrap();
    // Both slots are taken, so it waits.
    let [later] = submit(&owner, &["/c.bin"]).await[..] else {
        panic!()
    };
    assert_eq!(job(&owner, later).await.stage, JobStage::Queued);
    shutdown(owner).await;

    // The jobs in front lose their root and fill every slot the first
    // admission pass looks at; the one behind them still starts.
    env.roots.clear();
    let owner = env.start().await;
    for id in held {
        wait_for(&owner, id, |j| j.stage == JobStage::Blocked).await;
    }
    assert_eq!(complete(&owner, later).await.destination, Some(ROOT));
    assert_eq!(env.published("c.bin"), body(5 * KIB));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_submitted_before_the_download_root_is_mounted_is_saved_there() {
    let mut env = env();
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    env.server.put("/early.bin", Resource::new(body(5 * KIB)));
    // The download root's drive is not mounted yet, and nothing was chosen,
    // so there is no default: the job belongs to the download root.
    let owner = env.start_uninitialized().await;
    assert_eq!(settings(&owner).await.default_root, None);
    let [id] = submit(&owner, &["/early.bin"]).await[..] else {
        panic!()
    };
    let blocked = wait_for(&owner, id, |j| j.stage == JobStage::Blocked).await;
    assert_eq!(blocked.destination, None);

    // Another folder becomes the default before the drive is mounted.
    let revision = settings(&owner).await.revision;
    owner
        .handle()
        .execute(choose(&owner, revision, media_id), ALL)
        .await
        .unwrap();
    Root::initialize(&env.root, ROOT, owner.identity().instance).unwrap();
    // The download root cannot be removed while the job waits for it.
    let refused = owner
        .handle()
        .execute(
            remove_root(&owner, settings(&owner).await.revision, ROOT),
            ALL,
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable, "{refused:?}");
    assert!(
        refused.message.as_str().contains("unfinished"),
        "{refused:?}"
    );
    let paused = owner
        .handle()
        .execute(
            envelope(
                &owner,
                Command::Pause {
                    job: id,
                    expected_revision: blocked.revision,
                },
            ),
            ALL,
        )
        .await
        .unwrap();
    let CommandResult::JobUpdated { revision, .. } = paused else {
        panic!("{paused:?}")
    };
    let resume = Command::Resume {
        job: id,
        expected_revision: revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    // It still goes where it was meant to.
    assert_eq!(complete(&owner, id).await.destination, Some(ROOT));
    assert_eq!(env.published("early.bin"), body(5 * KIB));
    assert!(!media.join("early.bin").exists());
    shutdown(owner).await;
}

/// A hung mount: while it lasts, work on a folder named `stalled` blocks
/// at the failpoint until [`Hang::release`]. [`Hang::start`] blocks opening
/// the folder.
#[cfg(feature = "failpoints")]
struct Hang {
    point: &'static str,
    entered: tokio::sync::mpsc::UnboundedReceiver<()>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(feature = "failpoints")]
impl Hang {
    fn start() -> Self {
        Self::at("root-open-stall")
    }

    fn at(point: &'static str) -> Self {
        let (entered, rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let held = gate.clone();
        fail::cfg_callback(point, move || {
            let _ = entered.send(());
            let (open, released) = &*held;
            let mut open = open.lock().unwrap();
            while !*open {
                open = released.wait(open).unwrap();
            }
        })
        .unwrap();
        Self {
            point,
            entered: rx,
            gate,
        }
    }

    /// Waits until something is blocked in the folder.
    async fn entered(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.recv())
            .await
            .unwrap();
    }

    fn release(&self) {
        fail::remove(self.point);
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

#[cfg(feature = "failpoints")]
impl Drop for Hang {
    fn drop(&mut self) {
        self.release();
    }
}

/// The engine answers within this while a folder hangs.
#[cfg(feature = "failpoints")]
const PROMPT: Duration = Duration::from_secs(1);

/// The only test of the hung folder, since the failpoint is process-wide.
#[cfg(feature = "failpoints")]
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_that_hangs_delays_only_the_requests_about_it() {
    let mut env = env();
    env.server.put("/a.bin", Resource::new(body(5 * KIB)));
    env.server.put("/b.bin", Resource::new(body(5 * KIB)));
    let configured = folder(&env, "configured/stalled");
    env.roots.push(configured);
    let mut hang = Hang::start();

    // A configured folder that never answers costs startup at most one
    // probe deadline, and is then skipped while its check is stuck.
    let owner = env.start().await;
    hang.entered().await;
    let handle = owner.handle();
    let [a] = submit(&owner, &["/a.bin"]).await[..] else {
        panic!()
    };
    assert_eq!(complete(&owner, a).await.destination, Some(ROOT));
    tokio::time::timeout(PROMPT, handle.hello())
        .await
        .unwrap()
        .unwrap();

    // Registering a folder that hangs blocks only that registration.
    let chosen = folder(&env, "chosen/stalled");
    let registering = tokio::spawn({
        let (handle, chosen) = (handle.clone(), chosen.clone());
        async move { handle.register_root(chosen, ALL).await }
    });
    hang.entered().await;
    tokio::time::timeout(PROMPT, handle.hello())
        .await
        .unwrap()
        .unwrap();
    let listed = tokio::time::timeout(PROMPT, settings(&owner))
        .await
        .unwrap();
    assert!(
        listed
            .roots
            .iter()
            .all(|r| r.path != chosen.to_str().unwrap())
    );
    let [b] = tokio::time::timeout(PROMPT, submit(&owner, &["/b.bin"]))
        .await
        .unwrap()[..]
    else {
        panic!()
    };
    complete(&owner, b).await;
    // The same folder again is refused rather than queued behind it.
    let again = tokio::time::timeout(PROMPT, handle.register_root(chosen.clone(), ALL))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(again.code, ErrorCode::Busy);

    // Once the folder answers, the registration finishes.
    hang.release();
    let registered = registering.await.unwrap().unwrap();
    assert_eq!(registered.path, chosen.to_str().unwrap());
    assert!(
        settings(&owner)
            .await
            .roots
            .iter()
            .any(|r| r.id == registered.id)
    );

    // Finding a file to open in a folder that hangs, and cleaning up after
    // a download there, delay only themselves.
    let revision = settings(&owner).await.revision;
    handle
        .execute(choose(&owner, revision, registered.id), ALL)
        .await
        .unwrap();
    env.server.put("/c.bin", Resource::new(body(5 * KIB)));
    let [c] = submit(&owner, &["/c.bin"]).await[..] else {
        panic!()
    };
    drop(hang);
    // Published before the folder hangs again; under load it could
    // otherwise still be publishing, which opens the folder.
    complete(&owner, c).await;
    let mut hang = Hang::start();
    let locating = tokio::spawn({
        let handle = handle.clone();
        async move { handle.published_path(c, ALL).await }
    });
    hang.entered().await;
    tokio::time::timeout(PROMPT, handle.hello())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(PROMPT, job(&owner, c)).await.unwrap();
    hang.release();
    assert_eq!(locating.await.unwrap().unwrap(), chosen.join("c.bin"));

    // While a rename is recorded but has not yet moved the file, looking
    // the file up, or renaming it again, is refused rather than taking the
    // rename for one a crash interrupted.
    drop(hang);
    let mut hang = Hang::at("rename-recorded");
    let before = job(&owner, c).await;
    let renaming = tokio::spawn({
        let (handle, change) = (handle.clone(), rename(&owner, &before, "c2.bin"));
        async move { handle.execute(change, ALL).await }
    });
    hang.entered().await;
    let open = tokio::time::timeout(PROMPT, handle.published_path(c, ALL))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(open.code, ErrorCode::Busy);
    let again = tokio::time::timeout(PROMPT, refused(&owner, rename(&owner, &before, "c3.bin")))
        .await
        .unwrap();
    assert_eq!(again, ErrorCode::Busy);
    hang.release();
    renaming.await.unwrap().unwrap();
    assert!(!chosen.join("c.bin").exists() && !chosen.join("c3.bin").exists());
    assert_eq!(job(&owner, c).await.artifact.unwrap().path, "c2.bin");
    assert_eq!(
        handle.published_path(c, ALL).await.unwrap(),
        chosen.join("c2.bin")
    );

    // A registration still stuck in the folder at shutdown keeps the state
    // directory locked until the folder answers, and then stops without
    // writing to it, so no later owner of the directory races it.
    drop(hang);
    let mut hang = Hang::start();
    let late = folder(&env, "late/stalled");
    let registering = tokio::spawn({
        let (handle, late) = (handle.clone(), late.clone());
        async move { handle.register_root(late, ALL).await }
    });
    // The configured folder's next check hangs too; waits for both.
    hang.entered().await;
    let _ = tokio::time::timeout(Duration::from_millis(500), hang.entered()).await;
    use wayfetch_engine::ShutdownRequest;
    use wayfetch_engine::state_dir::{StateDir, StateDirError};
    let report = owner
        .shutdown(ShutdownRequest {
            // Past the probe deadline, so only the registration is left.
            timeout: Some(Duration::from_secs(3)),
        })
        .await;
    assert!(!report.clean && !report.state_dir_released, "{report:?}");
    assert!(matches!(
        StateDir::open(&env.state),
        Err(StateDirError::Locked(_))
    ));
    hang.release();
    let stopped = registering.await.unwrap().unwrap_err();
    assert_eq!(stopped.code, ErrorCode::ShuttingDown);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while StateDir::open(&env.state).is_err() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the lock was never released"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(std::fs::read_dir(&late).unwrap().next().is_none());
}

// Collected links: kept for review, started only when chosen, each once.

const SIGNED: &str = "https://files.example/b.bin?token=secret#part";

fn collect(owner: &EngineOwner, items: &[&str], destination: Option<RootId>) -> CommandEnvelope {
    envelope(
        owner,
        Command::CollectLinks {
            text: CollectText::try_from(items.join("\n")).unwrap(),
            base_url: None,
            mode: CollectMode::Analyze,
            destination,
        },
    )
}

fn refs(links: &[&CollectedLink]) -> LinkRefs {
    let refs = links.iter().map(|l| LinkRef {
        link: l.id,
        revision: l.revision,
    });
    LinkRefs::try_from(refs.collect::<Vec<_>>()).unwrap()
}

fn start_links(owner: &EngineOwner, links: &[&CollectedLink]) -> CommandEnvelope {
    envelope(
        owner,
        Command::StartLinks {
            routing: Some(wayfetch_protocol::command::StartRouting::Direct),
            links: refs(links),
            account: None,
        },
    )
}

async fn links(owner: &EngineOwner) -> LinkPage {
    let query = Query::ListLinks {
        after: None,
        page_size: PageSize::DEFAULT,
    };
    match owner.handle().query(query, ALL).await.unwrap() {
        QueryResult::Links(page) => page,
        other => panic!("{other:?}"),
    }
}

async fn applied(
    owner: &EngineOwner,
    envelope: CommandEnvelope,
) -> (Vec<AppliedLink>, Vec<(ErrorCode, String)>) {
    match owner.handle().execute(envelope, ALL).await.unwrap() {
        CommandResult::LinksApplied { applied, refused } => (
            applied,
            refused
                .into_iter()
                .map(|r| (r.code, r.message.as_str().to_owned()))
                .collect(),
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn collected_links_wait_for_review_and_survive_restart() {
    let env = env();
    env.server.put("/a.bin", Resource::new(body(5 * KIB)));
    env.server.put("/b.bin", Resource::new(body(6 * KIB)));
    let owner = env.start().await;
    // Once the download folder is identified, as reading the settings
    // ensures, a batch collected without a chosen folder is bound to the
    // default; before, it is left absent, which also means that folder.
    assert_eq!(settings(&owner).await.default_root, Some(ROOT));
    let collected = owner
        .handle()
        .execute(
            collect(
                &owner,
                &[
                    " https://files.example/a.bin ",
                    "ftp://user:pw@files.example/c?k=1",
                    SIGNED,
                ],
                None,
            ),
            ALL,
        )
        .await
        .unwrap();
    assert!(matches!(
        collected,
        CommandResult::LinksCollected {
            collected: 2,
            invalid: 1,
            ..
        }
    ));
    // Nothing was created or downloaded; this engine has no inspector, so
    // the links' checks wait.
    assert!(jobs(&owner).await.is_empty());
    assert_eq!(counted(&owner).await, 0);
    assert!(env.server.requests.lock().unwrap().is_empty());

    let page = links(&owner).await;
    assert_eq!((page.total, page.checking), (3, 2));
    let [a, c, b] = &page.items[..] else {
        panic!("{page:?}")
    };
    assert_eq!((a.batch, c.batch), (b.batch, b.batch));
    assert_eq!(
        (
            a.check,
            a.host.as_ref().unwrap().as_str(),
            a.name_hint.as_deref()
        ),
        (LinkCheck::Checking, "files.example", Some("a.bin"))
    );
    assert_eq!(a.destination, Some(ROOT));
    // The signed link is listed by host and name only; the whole link,
    // token included, needs the sources permission.
    assert_eq!(b.name_hint.as_deref(), Some("b.bin"));
    assert_eq!(b.text, None);
    let source = Query::LinkSource { link: b.id };
    let denied = owner
        .handle()
        .query(source.clone(), &[Permission::Read])
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    let QueryResult::LinkSource { url, .. } = owner.handle().query(source, ALL).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(url, SIGNED);
    // The invalid entry is kept with its problem, never its credentials.
    assert_eq!(c.check, LinkCheck::Invalid);
    assert_eq!(c.text.as_deref(), Some("ftp://files.example/c…"));
    assert!(
        c.problem.as_ref().unwrap().as_str().contains("http"),
        "{c:?}"
    );
    shutdown(owner).await;

    let owner = env.start().await;
    assert_eq!(links(&owner).await.items, page.items);
    assert!(jobs(&owner).await.is_empty());
    assert!(env.server.requests.lock().unwrap().is_empty());

    // Only the chosen link starts; the invalid one is refused and stays.
    let start = start_links(&owner, &[a, c]);
    let (done, refused) = applied(&owner, start.clone()).await;
    let [
        AppliedLink {
            link,
            job: Some(id),
            ..
        },
    ] = done[..]
    else {
        panic!("{done:?}")
    };
    assert_eq!(link, a.id);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].0, ErrorCode::NotApplicable);
    let finished = complete(&owner, id).await;
    assert_eq!(finished.destination, Some(ROOT));
    assert_eq!(env.published("a.bin"), body(5 * KIB));
    // Accepted links move out of Links atomically; refused links stay.
    let left = links(&owner).await;
    assert_eq!(left.total, 2);
    assert_eq!(left.items, [c.clone(), b.clone()]);
    assert!(
        env.server
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/a.bin")
    );

    // A retry, a double click with a fresh key, and a concurrent client
    // all find the same download.
    let (again, _) = applied(&owner, start).await;
    assert_eq!(again, done);
    let (twice, none) = applied(&owner, start_links(&owner, &[a])).await;
    assert_eq!((twice, none.len()), (done.clone(), 0));
    let (first, second) = tokio::join!(
        applied(&owner, start_links(&owner, &[b])),
        applied(&owner, start_links(&owner, &[b])),
    );
    assert_eq!(first.0, second.0);
    assert_eq!(jobs(&owner).await.len(), 2);
    shutdown(owner).await;
    let owner = env.start().await;
    let kept = links(&owner).await;
    assert_eq!(kept.total, 1);
    assert_eq!(kept.items.as_slice(), std::slice::from_ref(c));
    assert_eq!(jobs(&owner).await.len(), 2);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn link_destinations_change_only_the_chosen_links() {
    let mut env = env();
    let media = folder(&env, "media");
    env.roots.push(media.clone());
    let media_id = env.init_root(&media);
    env.server.put("/a.bin", Resource::new(body(5 * KIB)));
    let owner = env.start().await;
    owner
        .handle()
        .execute(
            collect(&owner, &["https://files.example/a.bin", SIGNED], None),
            ALL,
        )
        .await
        .unwrap();
    let page = links(&owner).await;
    let [a, b] = &page.items[..] else { panic!() };
    let before = settings(&owner).await;

    let to_media = |links: &[&CollectedLink], root| {
        envelope(
            &owner,
            Command::MoveLinks {
                links: refs(links),
                destination: root,
            },
        )
    };
    let (moved, none) = applied(&owner, to_media(&[a], media_id)).await;
    assert!(none.is_empty());
    let revision = a.revision.next().unwrap();
    assert_eq!(moved[0].revision, Some(revision));
    let page = links(&owner).await;
    assert_eq!(
        (page.items[0].destination, page.items[0].revision),
        (Some(media_id), revision)
    );
    // The other link, the default, and the settings revision keep theirs.
    assert_eq!(page.items[1], *b);
    let after = settings(&owner).await;
    assert_eq!(
        (after.revision, after.default_root),
        (before.revision, before.default_root)
    );

    // A start from the view before the move is refused, and the link stays.
    let (none, stale) = applied(&owner, start_links(&owner, &[a])).await;
    assert!(none.is_empty());
    assert_eq!(stale[0].0, ErrorCode::RevisionConflict);
    assert_eq!(links(&owner).await.total, 2);

    // Unknown and unavailable roots are refused whole.
    let unknown = RootId::from_u64(99).unwrap();
    assert_eq!(
        refused(&owner, to_media(&[b], unknown)).await,
        ErrorCode::NotFound
    );
    let mounted = folder(&env, "media-mounted");
    std::fs::remove_dir(&mounted).unwrap();
    std::fs::rename(&media, &mounted).unwrap();
    std::fs::create_dir(&media).unwrap();
    assert_eq!(
        refused(&owner, to_media(&[b], media_id)).await,
        ErrorCode::NotApplicable
    );
    assert_eq!(
        refused(&owner, collect(&owner, &[SIGNED], Some(media_id))).await,
        ErrorCode::NotApplicable
    );
    // A link whose folder went away is not started, and says why.
    let current = links(&owner).await.items[0].clone();
    let (none, unusable) = applied(&owner, start_links(&owner, &[&current])).await;
    assert!(none.is_empty());
    assert_eq!(unusable[0].0, ErrorCode::NotApplicable);
    let current = links(&owner).await.items[0].clone();
    assert!(
        current
            .problem
            .as_ref()
            .unwrap()
            .as_str()
            .contains("cannot be used")
    );
    assert!(jobs(&owner).await.is_empty());
    assert_eq!(std::fs::read_dir(&media).unwrap().count(), 0);

    // Remounted, it starts into the chosen folder.
    std::fs::remove_dir(&media).unwrap();
    std::fs::rename(&mounted, &media).unwrap();
    let (started, _) = applied(&owner, start_links(&owner, &[&current])).await;
    let done = complete(&owner, started[0].job.unwrap()).await;
    assert_eq!(done.destination, Some(media_id));
    assert_eq!(std::fs::read(media.join("a.bin")).unwrap(), body(5 * KIB));
    assert_eq!(links(&owner).await.total, 1);
    // The retained source identity no longer permits moving its destination.
    assert_eq!(
        refused_one(&owner, to_media(&[&current], ROOT)).await,
        ErrorCode::NotFound
    );
    shutdown(owner).await;
}

async fn refused_one(owner: &EngineOwner, envelope: CommandEnvelope) -> ErrorCode {
    let (done, refused) = applied(owner, envelope).await;
    assert!(done.is_empty(), "{done:?}");
    refused[0].0
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_links_removes_only_their_collected_records() {
    let env = env();
    env.server.put("/a.bin", Resource::new(body(5 * KIB)));
    let owner = env.start().await;
    owner
        .handle()
        .execute(
            collect(&owner, &["https://files.example/a.bin", SIGNED], None),
            ALL,
        )
        .await
        .unwrap();
    let page = links(&owner).await;
    let [a, b] = &page.items[..] else { panic!() };
    let (started, _) = applied(&owner, start_links(&owner, &[a])).await;
    let id = started[0].job.unwrap();
    complete(&owner, id).await;

    let remove =
        |links: &[&CollectedLink]| envelope(&owner, Command::RemoveLinks { links: refs(links) });
    // The list is shared: removing or moving entries needs control.
    for command in [
        remove(&[b]),
        envelope(
            &owner,
            Command::MoveLinks {
                links: refs(&[b]),
                destination: ROOT,
            },
        ),
    ] {
        let denied = owner
            .handle()
            .execute(command, &[Permission::Read, Permission::Submit])
            .await
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PermissionDenied);
    }
    assert_eq!(links(&owner).await.total, 1);
    // Removing an already moved source is harmless; the job keeps it.
    let (removed, refused_links) = applied(&owner, remove(&[a, b])).await;
    assert_eq!(removed.len(), 2);
    assert!(refused_links.is_empty());
    assert_eq!(links(&owner).await.total, 0);
    // The started download and its file are untouched.
    assert_eq!(job(&owner, id).await.stage, JobStage::Complete);
    assert_eq!(env.published("a.bin"), body(5 * KIB));
    // Removing again, or starting what was removed, changes nothing.
    let (again, none) = applied(&owner, remove(&[b])).await;
    assert_eq!((again.len(), none.len()), (1, 0));
    let (none, gone) = applied(&owner, start_links(&owner, &[b])).await;
    assert!(none.is_empty());
    assert_eq!(gone[0].0, ErrorCode::NotFound);
    assert_eq!(jobs(&owner).await.len(), 1);
    // Collecting needs the submit permission.
    let denied = owner
        .handle()
        .execute(collect(&owner, &[SIGNED], None), &[Permission::Read])
        .await
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::PermissionDenied);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_collected_list_is_bounded() {
    let env = env();
    let owner = env.start().await;
    let batch: Vec<String> = (0..MAX_SUBMIT_ITEMS)
        .map(|i| format!("https://files.example/{i}.bin"))
        .collect();
    let batch: Vec<&str> = batch.iter().map(String::as_str).collect();
    for _ in 0..MAX_COLLECTED_LINKS / MAX_SUBMIT_ITEMS {
        owner
            .handle()
            .execute(collect(&owner, &batch, None), ALL)
            .await
            .unwrap();
    }
    let full = owner
        .handle()
        .execute(collect(&owner, &[SIGNED], None), ALL)
        .await
        .unwrap_err();
    assert_eq!(full.code, ErrorCode::LimitExceeded);
    // Pages stay within their size, and walk the whole list in order.
    let mut after = None;
    let mut seen = 0;
    loop {
        let query = Query::ListLinks {
            after,
            page_size: PageSize::try_from(500).unwrap(),
        };
        let QueryResult::Links(page) = owner.handle().query(query, ALL).await.unwrap() else {
            panic!()
        };
        assert!(page.items.len() <= 500);
        assert_eq!(page.total, MAX_COLLECTED_LINKS as u64);
        seen += page.items.len();
        match page.next_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, MAX_COLLECTED_LINKS);
    assert!(jobs(&owner).await.is_empty());
    shutdown(owner).await;
}

// Manual retry: the same job again, continuing verified saved data when it
// safely can.

fn retry(owner: &EngineOwner, job: &JobSummary) -> CommandEnvelope {
    envelope(
        owner,
        Command::Retry {
            job: job.id,
            expected_revision: job.revision,
            repeat_provider_operation: false,
        },
    )
}

/// An environment whose stalled transfers fail quickly.
fn failing_env() -> Env {
    let mut env = env();
    env.timeouts = Timeouts {
        idle_read: Duration::from_millis(100),
        ..Timeouts::DEFAULT
    };
    env
}

/// Puts `data` at `path`, stalling at `at`, and waits for the job
/// submitted for it to fail with `at` bytes durable. The stall is then
/// lifted.
async fn failed_at(
    env: &Env,
    owner: &EngineOwner,
    path: &str,
    data: &Bytes,
    at: usize,
) -> JobSummary {
    let mut resource = Resource::new(data.clone());
    resource.stall_at = Some(at);
    env.server.put(path, resource);
    let [id] = submit(owner, &[path]).await[..] else {
        panic!()
    };
    let failed = wait_for(owner, id, |j| j.stage == JobStage::Failed).await;
    assert_eq!(failed.bytes_done, at as u64);
    assert_eq!(
        failed.failure.as_ref().unwrap().code,
        FailureCode::RequestTimeout
    );
    env.server.update(path, |r| r.stall_at = None);
    failed
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_transfer_retries_from_its_verified_offset() {
    let env = failing_env();
    let data = body(48 * KIB + 3);
    let owner = env.start().await;
    let failed = failed_at(&env, &owner, "/retry.bin", &data, 20 * KIB).await;
    assert_eq!(env.staging_files(), 1);

    let result = owner.handle().execute(retry(&owner, &failed), ALL).await;
    assert!(matches!(
        result,
        Ok(CommandResult::JobUpdated { changed: true, .. })
    ));
    let done = complete(&owner, failed.id).await;
    assert!(done.generation > failed.generation);
    assert_eq!(done.package, failed.package);
    assert_eq!(done.destination, failed.destination);
    assert_eq!(done.restart_reason, None);
    // Only the rest was asked for, on condition it is the same file.
    assert_eq!(
        env.server.ranges_requested(),
        vec![None, Some((20 * KIB as u64, None))]
    );
    let resumed = env.server.requests.lock().unwrap()[1].clone();
    assert_eq!(
        resumed.validators.unwrap().strong_etag.as_deref(),
        Some("\"v1\"")
    );
    assert_eq!(env.published("retry.bin"), data);
    assert_eq!(env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_partial_retries_in_this_session_and_after_restart() {
    let env = env();
    let data = body(32 * KIB);
    for path in ["/now.bin", "/later.bin"] {
        let mut resource = Resource::new(data.clone());
        resource.stall_at = Some(12 * KIB);
        env.server.put(path, resource);
    }
    let owner = env.start().await;
    let ids = submit(&owner, &["/now.bin", "/later.bin"]).await;
    for &id in &ids {
        wait_for(&owner, id, |j| j.bytes_done == 12 * KIB as u64).await;
        let summary = job(&owner, id).await;
        let cancel = Command::Cancel {
            job: id,
            expected_revision: summary.revision,
        };
        owner
            .handle()
            .execute(envelope(&owner, cancel), ALL)
            .await
            .unwrap();
        wait_for(&owner, id, |j| j.stage == JobStage::Cancelled).await;
    }
    for path in ["/now.bin", "/later.bin"] {
        env.server.update(path, |r| r.stall_at = None);
    }
    let cancelled = job(&owner, ids[0]).await;
    owner
        .handle()
        .execute(retry(&owner, &cancelled), ALL)
        .await
        .unwrap();
    complete(&owner, ids[0]).await;
    assert_eq!(env.published("now.bin"), data);
    shutdown(owner).await;

    // After a restart the partial is still kept, and a retry continues it.
    let owner = env.start().await;
    let cancelled = job(&owner, ids[1]).await;
    assert_eq!(cancelled.stage, JobStage::Cancelled);
    assert_eq!(cancelled.bytes_done, 12 * KIB as u64);
    owner
        .handle()
        .execute(retry(&owner, &cancelled), ALL)
        .await
        .unwrap();
    let done = complete(&owner, ids[1]).await;
    assert_eq!(done.restart_reason, None);
    assert_eq!(env.published("later.bin"), data);
    let continued = env
        .server
        .ranges_requested()
        .into_iter()
        .filter(|r| *r == Some((12 * KIB as u64, None)))
        .count();
    assert_eq!(continued, 2);
    assert_eq!(env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_and_stale_retries_start_one_attempt() {
    let env = failing_env();
    let data = body(24 * KIB);
    let owner = env.start().await;
    let failed = failed_at(&env, &owner, "/once.bin", &data, 8 * KIB).await;
    let handle = owner.handle();
    // Two clients retry from the same view at once.
    let (first, second) = (retry(&owner, &failed), retry(&owner, &failed));
    let (a, b) = tokio::join!(
        handle.execute(first.clone(), ALL),
        handle.execute(second.clone(), ALL)
    );
    let (accepted, refused) = match (a, b) {
        (Ok(result), Err(e)) => ((first, result), e),
        (Err(e), Ok(result)) => ((second, result), e),
        other => panic!("{other:?}"),
    };
    assert_eq!(refused.code, ErrorCode::RevisionConflict);
    assert!(refused.current_revision.is_some());
    // The accepted request's key replays its result, even once the job
    // has moved on.
    let done = complete(&owner, failed.id).await;
    let (envelope, result) = accepted;
    assert_eq!(handle.execute(envelope, ALL).await.unwrap(), result);
    assert_eq!(job(&owner, failed.id).await, done);
    // A retry from the old view, or of a finished job, changes nothing.
    assert_eq!(
        handle
            .execute(retry(&owner, &failed), ALL)
            .await
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        handle
            .execute(retry(&owner, &done), ALL)
            .await
            .unwrap_err()
            .code,
        ErrorCode::NotApplicable
    );
    // One attempt asked for the rest, once.
    assert_eq!(
        env.server.ranges_requested(),
        vec![None, Some((8 * KIB as u64, None))]
    );
    assert_eq!(env.published("once.bin"), data);
    assert!(!env.root.join("once (1).bin").exists());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_needs_control_and_this_instance() {
    let env = failing_env();
    let owner = env.start().await;
    let failed = failed_at(&env, &owner, "/denied.bin", &body(8 * KIB), 4 * KIB).await;
    let handle = owner.handle();
    let err = handle
        .execute(
            retry(&owner, &failed),
            &[Permission::Read, Permission::Submit],
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    let mut elsewhere = retry(&owner, &failed);
    elsewhere.target = wayfetch_protocol::InstanceId::generate();
    let err = handle.execute(elsewhere, ALL).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongInstance);
    assert_eq!(job(&owner, failed.id).await, failed);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_that_cannot_continue_starts_from_zero_and_says_why() {
    type Change = fn(&Env, &mut Resource);
    let cases: [(&str, Change, &str, bool); 5] = [
        (
            "changed",
            |_, r| r.etag = Some("\"v2\"".into()),
            "changed",
            true,
        ),
        ("ignores-range", |_, r| r.ranges = false, "whole file", true),
        (
            "unsatisfiable",
            |_, r| r.range_reply = Some(RangeReply::Unsatisfiable),
            "nothing was left",
            true,
        ),
        (
            "missing",
            |env, _| {
                for entry in std::fs::read_dir(env.root.join(".wayfetch/staging")).unwrap() {
                    std::fs::remove_file(entry.unwrap().path()).unwrap();
                }
            },
            "missing",
            false,
        ),
        (
            "replaced",
            |env, _| {
                for entry in std::fs::read_dir(env.root.join(".wayfetch/staging")).unwrap() {
                    let path = entry.unwrap().path();
                    let bytes = std::fs::read(&path).unwrap();
                    std::fs::remove_file(&path).unwrap();
                    std::fs::write(&path, bytes).unwrap();
                }
            },
            "replaced",
            false,
        ),
    ];
    for (name, change, reason, ranged) in cases {
        let env = failing_env();
        let path = format!("/{name}.bin");
        let data = body(40 * KIB);
        let owner = env.start().await;
        let failed = failed_at(&env, &owner, &path, &data, 16 * KIB).await;
        // The server's file is replaced by another of the same length.
        let fresh = if name == "changed" {
            body(40 * KIB).iter().map(|b| b ^ 0xff).collect::<Bytes>()
        } else {
            data.clone()
        };
        env.server.update(&path, |r| {
            r.body = fresh.clone();
            change(&env, r);
        });
        owner
            .handle()
            .execute(retry(&owner, &failed), ALL)
            .await
            .unwrap();
        let done = complete(&owner, failed.id).await;
        let why = done.restart_reason.expect(name);
        assert!(why.as_str().contains(reason), "{name}: {why}");
        // The file is one whole response, never saved bytes joined to
        // another file's.
        assert_eq!(env.published(&format!("{name}.bin")), fresh, "{name}");
        let mut expected = vec![None];
        if ranged {
            expected.push(Some((16 * KIB as u64, None)));
        }
        expected.push(None);
        assert_eq!(env.server.ranges_requested(), expected, "{name}");
        // A file that took the partial's name is not Wayfetch's to remove.
        let foreign = usize::from(name == "replaced");
        assert_eq!(env.staging_files(), foreign, "{name}");
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_range_on_retry_fails_without_writing() {
    let env = failing_env();
    let data = body(24 * KIB);
    let owner = env.start().await;
    let failed = failed_at(&env, &owner, "/malformed.bin", &data, 8 * KIB).await;
    env.server.update("/malformed.bin", |r| {
        r.range_reply = Some(RangeReply::WrongStart);
    });
    owner
        .handle()
        .execute(retry(&owner, &failed), ALL)
        .await
        .unwrap();
    let again = wait_for(&owner, failed.id, |j| {
        j.stage == JobStage::Failed && j.generation > failed.generation
    })
    .await;
    assert_eq!(
        again.failure.as_ref().unwrap().code,
        FailureCode::IntegrityMismatch
    );
    // The saved data is kept, unchanged, for another retry.
    assert_eq!(again.bytes_done, 8 * KIB as u64);
    assert_eq!(env.staging_files(), 1);
    assert!(!env.root.join("malformed.bin").exists());
    // Once the server answers properly, it continues from there.
    env.server
        .update("/malformed.bin", |r| r.range_reply = None);
    owner
        .handle()
        .execute(retry(&owner, &again), ALL)
        .await
        .unwrap();
    complete(&owner, failed.id).await;
    assert_eq!(env.published("malformed.bin"), data);
    assert_eq!(
        env.server.ranges_requested().last(),
        Some(&Some((8 * KIB as u64, None)))
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_into_an_unmounted_folder_blocks_without_writing() {
    let env = failing_env();
    let data = body(16 * KIB);
    let owner = env.start().await;
    let failed = failed_at(&env, &owner, "/unmounted.bin", &data, 4 * KIB).await;
    shutdown(owner).await;
    // The drive is gone: the root's sentinel no longer answers.
    let mounted = env.root.with_extension("mounted");
    std::fs::rename(&env.root, &mounted).unwrap();
    std::fs::create_dir(&env.root).unwrap();
    let owner = env.start_uninitialized().await;
    owner
        .handle()
        .execute(retry(&owner, &failed), ALL)
        .await
        .unwrap();
    let blocked = wait_for(&owner, failed.id, |j| j.stage == JobStage::Blocked).await;
    assert_eq!(
        blocked.failure.unwrap().code,
        FailureCode::DestinationUnavailable
    );
    assert!(std::fs::read_dir(&env.root).unwrap().next().is_none());
    assert_eq!(env.server.requests.lock().unwrap().len(), 1);
    shutdown(owner).await;
    // Back again, its saved data is still there to continue.
    std::fs::remove_dir(&env.root).unwrap();
    std::fs::rename(&mounted, &env.root).unwrap();
    let owner = env.start().await;
    let blocked = job(&owner, failed.id).await;
    assert_eq!(blocked.bytes_done, 4 * KIB as u64);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retried_download_keeps_its_source_and_stays_out_of_links_across_restart() {
    let env = failing_env();
    let data = body(20 * KIB);
    let mut resource = Resource::new(data.clone());
    resource.stall_at = Some(8 * KIB);
    env.server.put("/kept.bin", resource);
    let owner = env.start().await;
    owner
        .handle()
        .execute(
            collect(&owner, &["https://files.example/kept.bin"], None),
            ALL,
        )
        .await
        .unwrap();
    let page = links(&owner).await;
    let (applied, refused) = applied(&owner, start_links(&owner, &[&page.items[0]])).await;
    assert!(refused.is_empty());
    let id = applied[0].job.unwrap();
    let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    env.server.update("/kept.bin", |r| r.stall_at = None);
    let before = links(&owner).await.items;
    owner
        .handle()
        .execute(retry(&owner, &failed), ALL)
        .await
        .unwrap();
    complete(&owner, id).await;
    // Retry preserves the source without returning it to the collection.
    assert!(before.is_empty());
    assert!(links(&owner).await.items.is_empty());
    assert_eq!(jobs(&owner).await.len(), 1);
    shutdown(owner).await;
    let owner = env.start().await;
    assert!(links(&owner).await.items.is_empty());
    assert_eq!(env.published("kept.bin"), data);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_data_deletion_is_confined_permission_checked_and_replayable() {
    let env = env();
    env.server.put("/delete.bin", Resource::new(body(3 * KIB)));
    let owner = env.start().await;
    let id = submit(&owner, &["/delete.bin"]).await[0];
    let done = complete(&owner, id).await;
    let request = envelope(
        &owner,
        Command::DeleteFiles {
            job: id,
            expected_revision: done.revision,
        },
    );
    for grants in [&[Permission::Control][..], &[Permission::DeleteFiles][..]] {
        assert_eq!(
            owner
                .handle()
                .execute(request.clone(), grants)
                .await
                .unwrap_err()
                .code,
            ErrorCode::PermissionDenied
        );
        assert!(env.root.join("delete.bin").is_file());
    }
    std::fs::write(env.root.join("unrelated.bin"), b"user data").unwrap();
    let result = owner.handle().execute(request.clone(), ALL).await.unwrap();
    assert!(!env.root.join("delete.bin").exists());
    assert_eq!(
        std::fs::read(env.root.join("unrelated.bin")).unwrap(),
        b"user data"
    );
    assert_eq!(
        owner.handle().execute(request.clone(), ALL).await.unwrap(),
        result
    );
    // Permission is rechecked before returning a recorded destructive result.
    assert_eq!(
        owner
            .handle()
            .execute(request.clone(), &[Permission::Control])
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    shutdown(owner).await;
    let owner = env.start().await;
    assert_eq!(owner.handle().execute(request, ALL).await.unwrap(), result);
    assert_eq!(counted(&owner).await, 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_data_whose_folder_is_gone_removes_the_job() {
    let env = env();
    env.server.put("/emptied.bin", Resource::new(body(3 * KIB)));
    env.server
        .put("/vanished.bin", Resource::new(body(3 * KIB)));
    let owner = env.start().await;
    let ids = submit(&owner, &["/emptied.bin", "/vanished.bin"]).await;
    let emptied = complete(&owner, ids[0]).await;
    let vanished = complete(&owner, ids[1]).await;
    let delete = |job: &JobSummary| {
        envelope(
            &owner,
            Command::DeleteFiles {
                job: job.id,
                expected_revision: job.revision,
            },
        )
    };
    // Everything in the folder was deleted, Wayfetch's own data included.
    for entry in std::fs::read_dir(&env.root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
    owner.handle().execute(delete(&emptied), ALL).await.unwrap();
    assert_eq!(counted(&owner).await, 1);
    // Then the folder itself.
    std::fs::remove_dir(&env.root).unwrap();
    owner
        .handle()
        .execute(delete(&vanished), ALL)
        .await
        .unwrap();
    assert_eq!(counted(&owner).await, 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_data_deletion_keeps_job_visible_and_allows_an_explicit_keep_choice() {
    let env = env();
    env.server.put("/changed.bin", Resource::new(body(3 * KIB)));
    let owner = env.start().await;
    let id = submit(&owner, &["/changed.bin"]).await[0];
    let done = complete(&owner, id).await;
    let request = envelope(
        &owner,
        Command::DeleteFiles {
            job: id,
            expected_revision: done.revision,
        },
    );
    // Same name and length, different file identity.
    std::fs::write(env.root.join("replacement.bin"), body(3 * KIB)).unwrap();
    std::fs::rename(
        env.root.join("replacement.bin"),
        env.root.join("changed.bin"),
    )
    .unwrap();
    assert_eq!(
        refused(&owner, request.clone()).await,
        ErrorCode::NotApplicable
    );
    assert_eq!(job(&owner, id).await.stage, JobStage::Complete);
    assert!(env.root.join("changed.bin").is_file());
    assert_eq!(
        refused(&owner, rename(&owner, &done, "new.bin")).await,
        ErrorCode::Busy
    );
    // A fresh-key delete retry is safe and reports the same changed-file refusal.
    assert_eq!(
        refused(&owner, envelope(&owner, request.command.clone())).await,
        ErrorCode::NotApplicable
    );
    owner
        .handle()
        .execute(remove(&owner, &done), ALL)
        .await
        .unwrap();
    assert_eq!(counted(&owner).await, 0);
    assert!(env.root.join("changed.bin").is_file());
    assert_eq!(refused(&owner, request).await, ErrorCode::NotFound);
    shutdown(owner).await;
}
