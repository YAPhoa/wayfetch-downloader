//! Embedding the engine: a host starts, uses, and stops it through its
//! public API only, with no daemon or listener.

use std::time::Duration;

use tokio::runtime::Handle;
use wayfetch_engine::state_dir::StateDirError;
use wayfetch_engine::{
    EngineBuilder, EngineConfig, EngineOwner, Readiness, ShutdownRequest, StartError,
};
use wayfetch_protocol::{ErrorCode, ProtocolVersion};

async fn start(dir: &std::path::Path) -> Result<EngineOwner, StartError> {
    EngineBuilder::new(EngineConfig::new(dir), Handle::current())
        .start()
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn start_handshake_and_clean_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let owner = start(&dir.path().join("state")).await.unwrap();
    let handle = owner.handle();
    assert!(handle.wait_until_ready().await);

    let hello = handle.hello().await.unwrap();
    assert_eq!(hello.instance, owner.identity().instance);
    assert_eq!(hello.boot_epoch, owner.identity().boot_epoch);
    assert!(hello.protocol.serves(ProtocolVersion::CURRENT));
    assert_eq!(hello.head.instance, hello.instance);

    let report = owner.shutdown(ShutdownRequest::default()).await;
    assert!(report.clean, "{report:?}");
    assert_eq!(report.unfinished_tasks, 0);
    assert!(report.state_dir_released);
    assert!(report.elapsed < Duration::from_secs(5));

    // A handle that outlives the engine is refused, not hung.
    assert_eq!(handle.readiness(), Readiness::Stopped);
    assert_eq!(
        handle.hello().await.unwrap_err().code,
        ErrorCode::ShuttingDown
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_handles_does_not_stop_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let owner = start(dir.path()).await.unwrap();
    drop(owner.handle());
    drop(owner.handle());
    let handle = owner.handle();
    assert_eq!(handle.readiness(), Readiness::Ready);
    assert!(handle.hello().await.is_ok());
    assert!(owner.shutdown(ShutdownRequest::default()).await.clean);
}

#[tokio::test(flavor = "multi_thread")]
async fn second_engine_on_the_same_state_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = start(dir.path()).await.unwrap();
    let second = start(dir.path()).await;
    assert!(matches!(
        second,
        Err(StartError::StateDir(StateDirError::Locked(_)))
    ));
    // The running engine is unaffected.
    assert!(first.handle().hello().await.is_ok());
    assert!(first.shutdown(ShutdownRequest::default()).await.clean);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_keeps_identity_and_advances_boot_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let first = start(dir.path()).await.unwrap();
    let id = first.identity();
    assert!(first.shutdown(ShutdownRequest::default()).await.clean);

    let second = start(dir.path()).await.unwrap();
    assert_eq!(second.identity().instance, id.instance);
    assert_eq!(second.identity().boot_epoch, id.boot_epoch.next().unwrap());
    assert!(second.shutdown(ShutdownRequest::default()).await.clean);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_owner_releases_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let owner = start(dir.path()).await.unwrap();
    let handle = owner.handle();
    drop(owner);
    assert_eq!(handle.readiness(), Readiness::ShuttingDown);
    // Cancellation is asynchronous; the lock frees once the tasks end.
    let mut restarted = None;
    for _ in 0..100 {
        if let Ok(owner) = start(dir.path()).await {
            restarted = Some(owner);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        restarted
            .expect("lock released")
            .shutdown(ShutdownRequest::default())
            .await
            .clean
    );
}

/// A host without an async main builds its own runtime and blocks on the
/// engine API; the engine creates no runtime of its own.
#[test]
fn host_supplied_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let report = runtime.block_on(async {
        let owner = start(dir.path()).await.unwrap();
        owner.handle().hello().await.unwrap();
        owner.shutdown(ShutdownRequest::default()).await
    });
    assert!(report.clean);
}
