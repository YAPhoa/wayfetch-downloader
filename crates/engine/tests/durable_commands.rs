//! Durable commands and queries through the public engine API.

use tokio::runtime::Handle;
use wayfetch_engine::{EngineBuilder, EngineConfig, EngineOwner, ShutdownRequest};
use wayfetch_protocol::command::{InputUrl, SubmitItems};
use wayfetch_protocol::domain::job::{DisplayState, JobStage};
use wayfetch_protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, IdempotencyKey, Permission, Query,
    QueryResult,
};

const ALL: &[Permission] = &[Permission::Read, Permission::Submit, Permission::Control];

async fn start(dir: &std::path::Path) -> EngineOwner {
    EngineBuilder::new(EngineConfig::new(dir), Handle::current())
        .start()
        .await
        .unwrap()
}

fn submit(owner: &EngineOwner, key: IdempotencyKey) -> CommandEnvelope {
    let item = InputUrl::parse("https://files.example/a.bin").unwrap();
    CommandEnvelope {
        idempotency_key: key,
        target: owner.identity().instance,
        command: Command::Submit {
            items: SubmitItems::try_from(vec![item]).unwrap(),
            package: None,
        },
    }
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

#[tokio::test(flavor = "multi_thread")]
async fn commands_survive_restart_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let key = IdempotencyKey::generate();

    let owner = start(dir.path()).await;
    let handle = owner.handle();
    let submitted = handle.execute(submit(&owner, key), ALL).await.unwrap();
    // The input was staged into a queued job.
    let listed = jobs(&owner).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].stage, JobStage::Queued);
    let pause = CommandEnvelope {
        idempotency_key: IdempotencyKey::generate(),
        target: owner.identity().instance,
        command: Command::Pause {
            job: listed[0].id,
            expected_revision: listed[0].revision,
        },
    };
    let paused = handle.execute(pause.clone(), ALL).await.unwrap();
    let head = handle.hello().await.unwrap().head;
    assert!(head.position >= 3, "{head:?}");
    assert!(owner.shutdown(ShutdownRequest::default()).await.clean);

    let owner = start(dir.path()).await;
    let handle = owner.handle();
    // Retries after the restart replay the stored results and change nothing.
    assert_eq!(
        handle.execute(submit(&owner, key), ALL).await.unwrap(),
        submitted
    );
    assert_eq!(handle.execute(pause, ALL).await.unwrap(), paused);
    let listed = jobs(&owner).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].display, DisplayState::Stage(JobStage::Paused));
    assert_eq!(handle.hello().await.unwrap().head.position, head.position);
    assert!(matches!(
        submitted,
        CommandResult::Submitted { accepted: 1, .. }
    ));
    assert!(owner.shutdown(ShutdownRequest::default()).await.clean);
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_and_target_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let owner = start(dir.path()).await;
    let handle = owner.handle();
    let err = handle
        .execute(
            submit(&owner, IdempotencyKey::generate()),
            &[Permission::Read],
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);
    let query = Query::InspectJob {
        job: wayfetch_protocol::domain::id::JobId::from_u64(1).unwrap(),
    };
    let err = handle.query(query, &[]).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);

    let mut elsewhere = submit(&owner, IdempotencyKey::generate());
    elsewhere.target = wayfetch_protocol::InstanceId::generate();
    let err = handle.execute(elsewhere, ALL).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongInstance);
    assert!(owner.shutdown(ShutdownRequest::default()).await.clean);
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_the_durability_profile() {
    let dir = tempfile::tempdir().unwrap();
    let owner = start(dir.path()).await;
    let info = owner.store_info();
    assert_eq!(info.journal_mode, "wal");
    assert_eq!(info.synchronous, 2, "synchronous=FULL");
    let version: Vec<u32> = info
        .sqlite_version
        .split('.')
        .map(|p| p.parse().unwrap())
        .collect();
    assert!(version >= vec![3, 51, 3], "{}", info.sqlite_version);
    assert!(owner.shutdown(ShutdownRequest::default()).await.clean);
}
