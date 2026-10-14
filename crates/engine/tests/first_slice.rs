//! The first end-to-end slice (architecture brief section 14.3) through the
//! public engine API: submit one direct link, persist the job, download it
//! into owned staging, verify, publish, restart the engine, and list the
//! completed artifact.
//!
//! Bytes come from an in-process [`Transport`]; requests over a real
//! socket arrive with the HTTP client. Brief tests exercised here at the
//! engine level: T01 (sequential file), T02 and T23 (resume from durable
//! checkpoints after a restart), T03 (`200` to a range request), T05
//! (representation changed during resume), T09 (stalled headers and body),
//! T27 (cancelled generation), and T47 (bounded subscriber replay).

use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::runtime::Handle;
use wayfetch_engine::fs::Root;
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_engine::{
    EngineBuilder, EngineConfig, EngineOwner, Limits, RootConfig, ShutdownRequest, Timeouts,
};
use wayfetch_protocol::command::{InputUrl, SubmitItems};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::{JobId, RootId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::event::{EventKind, ResyncReason};
use wayfetch_protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_protocol::{
    Command, CommandEnvelope, IdempotencyKey, Permission, Query, QueryResult, StreamItem, Subscribe,
};

const ALL: &[Permission] = &[Permission::Read, Permission::Submit, Permission::Control];
const ROOT: RootId = RootId::from_u64(1).unwrap();
const KIB: usize = 1024;

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
        }
    }

    fn representation(&self) -> Representation {
        Representation {
            length: Some(self.body.len() as u64),
            strong_etag: self.etag.clone(),
            last_modified: None,
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
            let (framing, start) = match request.range {
                Some((start, None)) if resource.ranges && matches => (
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

/// A state directory and an initialized download root.
struct Env {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    root: PathBuf,
    server: Arc<Server>,
    limits: Limits,
    timeouts: Timeouts,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (state, root) = (tmp.path().join("state"), tmp.path().join("downloads"));
        std::fs::create_dir(&root).unwrap();
        Self {
            _tmp: tmp,
            state,
            root,
            server: Arc::default(),
            limits: Limits {
                read_buffer_bytes: NonZeroUsize::new(4 * KIB).unwrap(),
                checkpoint_bytes: NonZeroU64::new(4 * KIB as u64).unwrap(),
                ..Limits::DEFAULT
            },
            timeouts: Timeouts::DEFAULT,
        }
    }

    async fn start(&self) -> EngineOwner {
        let mut config = EngineConfig::new(&self.state);
        config.roots.push(RootConfig {
            id: ROOT,
            path: self.root.clone(),
        });
        config.download_root = Some(ROOT);
        config.limits = self.limits.clone();
        config.timeouts = self.timeouts;
        let owner = EngineBuilder::new(config, Handle::current())
            .transport(Arc::new(Fake(self.server.clone())))
            .start()
            .await
            .unwrap();
        // An operator action, once per root and instance.
        Root::initialize(&self.root, ROOT, owner.identity().instance).unwrap();
        owner
    }

    fn published(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.root.join(path)).unwrap()
    }
}

fn body(len: usize) -> Bytes {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn envelope(owner: &EngineOwner, command: Command) -> CommandEnvelope {
    CommandEnvelope {
        idempotency_key: IdempotencyKey::generate(),
        target: owner.identity().instance,
        command,
    }
}

/// Submits `paths` on the fake server and returns the new jobs.
async fn submit(owner: &EngineOwner, paths: &[&str]) -> Vec<JobId> {
    let before = jobs(owner).await.len();
    let items = paths
        .iter()
        .map(|p| InputUrl::parse(&format!("https://files.example{p}")).unwrap())
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

async fn jobs(owner: &EngineOwner) -> Vec<JobSummary> {
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

async fn job(owner: &EngineOwner, job: JobId) -> JobSummary {
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
async fn wait_for(
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

async fn complete(owner: &EngineOwner, id: JobId) -> JobSummary {
    wait_for(owner, id, |j| {
        assert!(
            !matches!(j.stage, JobStage::Failed | JobStage::Blocked),
            "{j:?}"
        );
        j.stage == JobStage::Complete
    })
    .await
}

async fn shutdown(owner: EngineOwner) {
    let report = owner.shutdown(ShutdownRequest::default()).await;
    assert!(report.clean, "{report:?}");
}

fn staging_files(root: &Path) -> usize {
    std::fs::read_dir(root.join(".wayfetch/staging"))
        .unwrap()
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_download_is_published_and_listed_after_restart() {
    let env = Env::new();
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
    assert_eq!(staging_files(&env.root), 0);
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
    let env = Env::new();
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
    let env = Env::new();
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
    assert_eq!(staging_files(&env.root), 0);
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
        let env = Env::new();
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
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_headers_and_bodies_fail_at_their_deadlines() {
    let mut env = Env::new();
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
    let env = Env::new();
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
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn active_files_bounds_concurrent_transfers() {
    let mut env = Env::new();
    env.limits.active_files = NonZeroUsize::new(2).unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn subscribers_replay_within_bounds_and_resync_beyond_them() {
    let mut env = Env::new();
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
        let Some(StreamItem::Event(event)) = live.next().await else {
            panic!("stream ended early")
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
    let mut env = Env::new();
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
    let mut env = Env::new();
    env.limits.active_files = NonZeroUsize::new(1).unwrap();
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
    let env = Env::new();
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
    assert_eq!(staging_files(&env.root), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_pause_does_not_interrupt_a_resumed_transfer() {
    let env = Env::new();
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
