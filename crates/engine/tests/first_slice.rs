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
//! last tests cover destination roots and the default for new jobs.

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
use wayfetch_engine::{EngineOwner, Timeouts};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::{JobId, RootId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::version::Revision;
use wayfetch_protocol::event::{EventKind, ResyncReason};
use wayfetch_protocol::query::{JobFilter, PageSize, Settings};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, Permission, Query, QueryResult, StreamItem,
    Subscribe,
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
async fn active_files_bounds_concurrent_transfers() {
    let mut env = env();
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
            default_root: root,
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

#[tokio::test(flavor = "multi_thread")]
async fn blocked_jobs_do_not_hold_back_the_jobs_behind_them() {
    let mut env = env();
    env.limits.active_files = NonZeroUsize::new(2).unwrap();
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

/// A hung mount: while it lasts, opening a folder named `stalled` blocks
/// until [`Hang::release`].
#[cfg(feature = "failpoints")]
struct Hang {
    entered: tokio::sync::mpsc::UnboundedReceiver<()>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

#[cfg(feature = "failpoints")]
impl Hang {
    fn start() -> Self {
        let (entered, rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let held = gate.clone();
        fail::cfg_callback("root-open-stall", move || {
            let _ = entered.send(());
            let (open, released) = &*held;
            let mut open = open.lock().unwrap();
            while !*open {
                open = released.wait(open).unwrap();
            }
        })
        .unwrap();
        Self { entered: rx, gate }
    }

    /// Waits until something is blocked in the folder.
    async fn entered(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.recv())
            .await
            .unwrap();
    }

    fn release(&self) {
        fail::remove("root-open-stall");
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
    shutdown(owner).await;
}
