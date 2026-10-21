//! The engine downloading over real sockets through its public API: HTTPS
//! transfers, resume after a restart and its refusals, failures that must
//! not publish, the egress policy, and the shared speed limit.

#[path = "../../engine/tests/support/mod.rs"]
mod engine_support;
mod support;

use std::time::Duration;

use bytes::Bytes;
use engine_support::{CHECKPOINT, KIB, body, complete, jobs, settled, shutdown, wait_for};
use sha2::{Digest, Sha256};
use support::{Raw, Resource, Route, Server, https_env};
use wayfetch_engine::protocol::query::SpeedLimit;
use wayfetch_engine::transfer::throttle::quantum;
use wayfetch_engine::{EngineOwner, Timeouts};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::JobId;
use wayfetch_protocol::domain::job::JobStage;

type Env = engine_support::Env<Server>;

async fn env() -> Env {
    https_env(|_| {}).await
}

/// Submits `paths` on the server and returns the new jobs.
async fn submit(env: &Env, owner: &EngineOwner, paths: &[&str]) -> Vec<JobId> {
    engine_support::submit(owner, paths.iter().map(|p| env.server.url(p))).await
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Where the resume after an interruption at `at` started, checking that it
/// continued from the durable bytes.
fn resumed_from(range: Option<(u64, Option<u64>)>, at: usize) -> u64 {
    let Some((start, None)) = range else {
        panic!("not a resume: {range:?}")
    };
    assert!(settled(start, at) && start <= at as u64, "{start}");
    start
}

/// Starts a download that stalls at `at` bytes, waits until its last
/// checkpoint committed, and stops the engine with the transfer still
/// open, as an interruption leaves it.
async fn interrupt(owner: EngineOwner, id: JobId, at: usize) {
    wait_for(&owner, id, |j| settled(j.bytes_done, at)).await;
    shutdown(owner).await;
}

/// Serves `resource` at `path` stalled at `at`, and interrupts its download.
async fn interrupt_at(env: &Env, path: &str, resource: Resource, at: usize) -> JobId {
    env.server.put(
        path,
        Resource {
            stall_at: Some(at),
            ..resource
        },
    );
    let owner = env.start().await;
    let [id] = submit(env, &owner, &[path]).await[..] else {
        panic!()
    };
    interrupt(owner, id, at).await;
    env.server.update(path, |r| r.stall_at = None);
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_https_download_resumes_and_publishes_after_restart() {
    let env = env().await;
    let data = body(200 * KIB + 11);
    let at = 72 * KIB;
    let id = interrupt_at(&env, "/big.bin", Resource::new(data.clone()), at).await;

    let owner = env.start().await;
    let done = complete(&owner, id).await;
    // Recovery adopted the partial under a new generation and asked only
    // for the rest, conditioned on the representation it had.
    assert!(done.generation.get() > 1);
    let ranges = env.server.ranges("/big.bin");
    assert_eq!((ranges.len(), ranges[0]), (2, None));
    resumed_from(ranges[1], at);
    let resumed = &env.server.log()[1];
    assert_eq!(resumed.header("if-range"), Some("\"v1\""));
    let artifact = done.artifact.clone().expect("published");
    assert_eq!(
        (artifact.path.as_str(), artifact.size),
        ("big.bin", data.len() as u64)
    );
    assert_eq!(digest(&env.published("big.bin")), digest(&data));
    assert_eq!(env.staging_files(), 0);
    shutdown(owner).await;

    // Another restart lists the job as complete and requests nothing.
    let owner = env.start().await;
    assert_eq!(jobs(&owner).await, vec![done]);
    assert_eq!(env.server.hits("/big.bin"), 2);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_files_and_suggested_names_publish() {
    let env = env().await;
    env.server.put(
        "/download?id=7",
        Resource::new(Bytes::new()).header(
            "Content-Disposition",
            "attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.txt",
        ),
    );
    let owner = env.start().await;
    let [id] = submit(&env, &owner, &["/download?id=7"]).await[..] else {
        panic!()
    };
    let done = complete(&owner, id).await;
    // T01: a valid empty file, named by the server.
    assert_eq!(done.artifact.unwrap().path, "résumé.txt");
    assert_eq!(env.published("résumé.txt"), b"");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_the_server_does_not_honor_restarts_from_zero() {
    type Change = fn(&mut Resource);
    let cases: [(&str, Change); 4] = [
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
        // T04: `416` claims nothing is left though the partial is short.
        ("unsatisfiable", |r| {
            r.unsatisfiable = Some(r.body.len() as u64)
        }),
    ];
    for (name, change) in cases {
        let env = env().await;
        let path = format!("/{name}.bin");
        let data = body(64 * KIB);
        let at = 20 * KIB;
        let id = interrupt_at(&env, &path, Resource::new(data.clone()), at).await;
        env.server.update(&path, change);

        let owner = env.start().await;
        let done = complete(&owner, id).await;
        let ranges = env.server.ranges(&path);
        assert_eq!(
            (ranges.len(), ranges[0], ranges[2]),
            (3, None, None),
            "{name}"
        );
        resumed_from(ranges[1], at);
        // The artifact is the whole representation from one response, not
        // old bytes with new ones appended.
        assert_eq!(env.published(&format!("{name}.bin")), data, "{name}");
        assert_eq!(done.artifact.unwrap().size, data.len() as u64, "{name}");
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unsatisfiable_finishes_only_complete_durable_coverage() {
    let env = env().await;
    // One checkpoint's worth, so all of it is durable once it arrived.
    let data = body(CHECKPOINT);
    // No `Content-Length`: the engine holds every byte but cannot know it
    // has all of them until the server says so; the connection stays open.
    let mut resource = Resource::new(data.clone());
    resource.length_header = false;
    resource.stall_at = Some(data.len());
    env.server.put("/whole.bin", resource);
    let owner = env.start().await;
    let [id] = submit(&env, &owner, &["/whole.bin"]).await[..] else {
        panic!()
    };
    wait_for(&owner, id, |j| j.bytes_done == data.len() as u64).await;
    shutdown(owner).await;
    env.server
        .update("/whole.bin", |r| r.unsatisfiable = Some(data.len() as u64));

    let owner = env.start().await;
    complete(&owner, id).await;
    // T04: `416` for the next byte confirms the durable coverage is the
    // whole representation; nothing is downloaded again.
    assert_eq!(
        env.server.ranges("/whole.bin"),
        vec![None, Some((data.len() as u64, None))]
    );
    assert_eq!(env.published("whole.bin"), data);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_redirected_to_another_origin_restarts() {
    let env = env().await;
    let data = body(48 * KIB);
    let at = 16 * KIB;
    env.server
        .route("/link", Route::Redirect(302, env.server.url("/file.bin")));
    env.server.put(
        "/file.bin",
        Resource {
            stall_at: Some(at),
            ..Resource::new(data.clone())
        },
    );
    let owner = env.start().await;
    let [id] = submit(&env, &owner, &["/link"]).await[..] else {
        panic!()
    };
    interrupt(owner, id, at).await;
    env.server.update("/file.bin", |r| r.stall_at = None);
    // The link now leads to another origin serving the same entity tag.
    env.server.route(
        "/link",
        Route::Redirect(302, env.server.url_via("localhost", "/file.bin")),
    );

    let owner = env.start().await;
    complete(&owner, id).await;
    // T06: the matching validator from another origin does not make its
    // bytes equivalent; the job started over from zero.
    let ranges = env.server.ranges("/file.bin");
    assert_eq!((ranges.len(), ranges[0], ranges[2]), (3, None, None));
    resumed_from(ranges[1], at);
    let resumed = &env.server.log()[3];
    assert_eq!(
        resumed.header("host"),
        Some(format!("localhost:{}", env.server.port())).as_deref()
    );
    // The artifact is named after the submitted link.
    assert_eq!(env.published("link"), data);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resume_redirected_to_another_resource_on_the_same_origin_restarts() {
    let env = env().await;
    let (old, new) = (body(48 * KIB), body(48 * KIB + 1).slice(1..));
    assert_ne!(old, new);
    let at = 16 * KIB;
    env.server
        .route("/link", Route::Redirect(302, env.server.url("/old.bin")));
    env.server.put(
        "/old.bin",
        Resource {
            stall_at: Some(at),
            ..Resource::new(old)
        },
    );
    let owner = env.start().await;
    let [id] = submit(&env, &owner, &["/link"]).await[..] else {
        panic!()
    };
    interrupt(owner, id, at).await;
    // The link now leads to another file on the same server, of the same
    // length and with the same entity tag.
    env.server.put("/new.bin", Resource::new(new.clone()));
    env.server
        .route("/link", Route::Redirect(302, env.server.url("/new.bin")));

    let owner = env.start().await;
    complete(&owner, id).await;
    // T06: an entity tag identifies representations of one resource only;
    // the range the other file answered was discarded and the job started
    // over from zero.
    let ranges = env.server.ranges("/new.bin");
    assert_eq!((ranges.len(), ranges[1]), (2, None));
    resumed_from(ranges[0], at);
    assert_eq!(env.published("link"), new);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stalled_and_truncated_transfers_fail_without_publishing() {
    let mut env = env().await;
    env.timeouts = Timeouts {
        connect: Duration::from_millis(500),
        response: Duration::from_millis(500),
        idle_read: Duration::from_millis(300),
    };
    env.server.route("/no-headers.bin", Route::Raw(Raw::Hang));
    env.server.put(
        "/stalled.bin",
        Resource {
            stall_at: Some(8 * KIB),
            ..Resource::new(body(16 * KIB))
        },
    );
    env.server.put(
        "/truncated.bin",
        Resource {
            drop_at: Some(8 * KIB),
            ..Resource::new(body(16 * KIB))
        },
    );
    let owner = env.start().await;
    let paths = ["/no-headers.bin", "/stalled.bin", "/truncated.bin"];
    let ids = submit(&env, &owner, &paths).await;
    for (id, path) in ids.into_iter().zip(paths) {
        // T09 and T08: each deadline or the short body fails the job; no
        // partial is published.
        let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
        assert_eq!(
            failed.failure.unwrap().code,
            FailureCode::RequestTimeout,
            "{path}"
        );
        assert!(failed.artifact.is_none(), "{path}");
        assert!(!env.root.join(&path[1..]).exists(), "{path}");
    }
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn destinations_outside_the_egress_policy_are_never_contacted() {
    // No allowlist: the local server is not a permitted destination.
    let env = https_env(|config| config.egress.allowed_networks.clear()).await;
    env.server.put("/a.bin", Resource::new(body(KIB)));
    let owner = env.start().await;
    let [id] = submit(&env, &owner, &["/a.bin"]).await[..] else {
        panic!()
    };
    let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    assert_eq!(
        failed.failure.unwrap().code,
        FailureCode::UnsupportedCapability
    );
    assert!(env.server.log().is_empty());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_https_downloads_share_the_total_speed_limit() {
    let mut env = env().await;
    let limit = 256 * KIB;
    env.policy.download_speed_limit.default = Some(SpeedLimit::BytesPerSecond(limit as u64));
    let data = body(192 * KIB);
    let paths = ["/one.bin", "/two.bin", "/three.bin"];
    for path in paths {
        env.server.put(path, Resource::new(data.clone()));
    }
    let owner = env.start().await;
    let started = tokio::time::Instant::now();
    let ids = submit(&env, &owner, &paths).await;
    for id in ids {
        let done = complete(&owner, id).await;
        assert_eq!(done.bytes_done, data.len() as u64);
    }
    let elapsed = started.elapsed();
    // Starting without credit, the engine admits at most one quantum more
    // than the limit allows: 576 KiB at 256 KiB/s take over two seconds,
    // where each transfer alone at the limit would have taken under one.
    let floor = (3 * data.len() - quantum(limit as u64) as usize) as f64 / limit as f64;
    assert!(elapsed.as_secs_f64() >= floor, "{elapsed:?} < {floor}s");
    for path in paths {
        assert_eq!(
            digest(&env.published(path.trim_start_matches('/'))),
            digest(&data)
        );
        assert_eq!(env.server.hits(path), 1);
    }
    shutdown(owner).await;
}
