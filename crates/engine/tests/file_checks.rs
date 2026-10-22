//! Checking complete downloads against checksum files downloaded with them,
//! and downloading a file that failed again, through the public engine API.

mod support;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, envelope, job, shutdown, wait_for};
use tokio_util::sync::CancellationToken;
use wayfetch_engine::EngineOwner;
use wayfetch_engine::checksum::{self, Algorithm};
use wayfetch_engine::extract::{Archive, Entry, EntryKind, ExtractError, Extractor, Halt};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_protocol::domain::id::JobId;
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::media::{PluginRole, PluginState, PluginSummary};
use wayfetch_protocol::query::{ExtractionState, FileCheckState, JobSummary, Settings};
use wayfetch_protocol::{Command, CommandResult, ErrorCode, ProtocolError, Query, QueryResult};

/// Files served by path, counting requests; a held path's body does not
/// end until it is released.
#[derive(Default)]
struct Files {
    bodies: Mutex<HashMap<String, Bytes>>,
    requests: Mutex<HashMap<String, usize>>,
    held: Mutex<HashSet<String>>,
}

impl Files {
    fn serve(&self, path: &str, body: impl Into<Bytes>) {
        self.bodies
            .lock()
            .unwrap()
            .insert(format!("/{path}"), body.into());
    }

    fn hold(&self, path: &str, held: bool) {
        let mut paths = self.held.lock().unwrap();
        if held {
            paths.insert(format!("/{path}"));
        } else {
            paths.remove(&format!("/{path}"));
        }
    }

    fn requests(&self, path: &str) -> usize {
        self.requests
            .lock()
            .unwrap()
            .get(&format!("/{path}"))
            .copied()
            .unwrap_or(0)
    }
}

struct Body {
    files: Arc<Files>,
    path: String,
    data: Option<Bytes>,
}

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
        Box::pin(async move {
            while self.files.held.lock().unwrap().contains(&self.path) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(self.data.take())
        })
    }
}

struct Served(Arc<Files>);

impl Transport for Served {
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        Box::pin(async move {
            let path = request.url.path().to_owned();
            *self
                .0
                .requests
                .lock()
                .unwrap()
                .entry(path.clone())
                .or_default() += 1;
            let data = self
                .0
                .bodies
                .lock()
                .unwrap()
                .get(&path)
                .cloned()
                .unwrap_or_else(|| Bytes::from(format!("file at {path}")));
            Ok(Response {
                framing: Framing::Full {
                    length: Some(data.len() as u64),
                },
                representation: Representation {
                    length: Some(data.len() as u64),
                    strong_etag: None,
                    last_modified: None,
                    resource: None,
                },
                filename: None,
                body: Box::new(Body {
                    files: self.0.clone(),
                    path,
                    data: Some(data),
                }),
            })
        })
    }
}

/// An extractor that records what it unpacked.
#[derive(Default)]
struct Unpacker {
    started: Mutex<Vec<String>>,
}

impl Extractor for Unpacker {
    fn plugin(&self) -> PluginSummary {
        PluginSummary {
            id: "fake-unpacker".into(),
            name: "Fake unpacker".into(),
            role: PluginRole::ArchiveExtractor,
            services: Vec::new(),
            state: PluginState::Ready,
            version: Some("1.0".into()),
            problem: None,
            enabled: true,
            updatable: false,
            installed: false,
            fallback_version: None,
        }
    }

    fn list<'a>(
        &'a self,
        _archive: Archive<'a>,
        _halt: Halt,
    ) -> BoxFuture<'a, Result<Vec<Entry>, ExtractError>> {
        Box::pin(async move {
            Ok(vec![Entry {
                path: "only.txt".into(),
                kind: EntryKind::File,
                size: Some(2),
                encrypted: false,
            }])
        })
    }

    fn extract<'a>(
        &'a self,
        archive: Archive<'a>,
        output: &'a Path,
        _budget: u64,
        _halt: Halt,
    ) -> BoxFuture<'a, Result<(), ExtractError>> {
        Box::pin(async move {
            let name = archive.path.file_name().unwrap().to_string_lossy();
            self.started.lock().unwrap().push(name.into_owned());
            std::fs::write(output.join("only.txt"), b"ok").unwrap();
            Ok(())
        })
    }
}

type Env = support::Env<Arc<Files>>;

fn env() -> Env {
    let files = Arc::<Files>::default();
    Env::new(files.clone(), Arc::new(Served(files)))
}

fn crc(data: &[u8]) -> String {
    let digests = checksum::hash(
        &mut &data[..],
        &[Algorithm::Crc32],
        &CancellationToken::new(),
    )
    .unwrap()
    .unwrap();
    digests[0].value().to_owned()
}

async fn submit(owner: &EngineOwner, names: &[&str]) -> Vec<JobId> {
    support::submit(
        owner,
        names.iter().map(|n| format!("https://files.example/{n}")),
    )
    .await
}

fn check(job: &JobSummary) -> Option<FileCheckState> {
    job.check.as_ref().map(|c| c.state)
}

/// Waits until `id` is complete and its check is `wanted`.
async fn checked(owner: &EngineOwner, id: JobId, wanted: FileCheckState) -> JobSummary {
    wait_for(owner, id, |j| {
        j.stage == JobStage::Complete && check(j) == Some(wanted)
    })
    .await
}

async fn command(owner: &EngineOwner, command: Command) -> Result<CommandResult, ProtocolError> {
    owner.handle().execute(envelope(owner, command), ALL).await
}

async fn retry(owner: &EngineOwner, id: JobId) -> Result<CommandResult, ProtocolError> {
    let current = job(owner, id).await;
    command(
        owner,
        Command::Retry {
            job: id,
            expected_revision: current.revision,
            repeat_provider_operation: false,
        },
    )
    .await
}

async fn settings(owner: &EngineOwner) -> Settings {
    match owner.handle().query(Query::Settings {}, ALL).await.unwrap() {
        QueryResult::Settings(settings) => settings,
        other => panic!("{other:?}"),
    }
}

async fn redownload(owner: &EngineOwner, on: bool) {
    let current = settings(owner).await;
    assert!(current.check_files);
    command(
        owner,
        Command::UpdateSettings {
            expected_revision: current.revision,
            default_root: None,
            max_active_downloads: None,
            download_speed_limit: None,
            extract_archives: None,
            automatic_update_checks: None,
            check_files: None,
            redownload_failed_checks: Some(on),
        },
    )
    .await
    .unwrap();
    assert_eq!(settings(owner).await.redownload_failed_checks, on);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checksum_file_checks_its_batch_and_a_failed_file_is_kept_until_retried() {
    let env = env();
    env.server.serve("a.bin", "alpha");
    env.server.serve("b.bin", "bravo");
    env.server.serve(
        "release.sfv",
        format!(
            "; made by a tool\r\na.bin {}\r\nB.BIN {}\r\nmissing.bin 00000000\r\n",
            crc(b"alpha"),
            crc(b"charlie")
        ),
    );
    let owner = env.start().await;
    // Off by default: a file that fails is kept until the person retries.
    assert!(!settings(&owner).await.redownload_failed_checks);
    let ids = submit(&owner, &["a.bin", "b.bin", "release.sfv"]).await;
    let a = checked(&owner, ids[0], FileCheckState::Verified).await;
    let a_check = a.check.unwrap();
    assert_eq!(a_check.algorithm, Algorithm::Crc32);
    assert_eq!(a_check.source.as_deref(), Some("release.sfv"));
    // A name in another case matches when only one download has it.
    let b = checked(&owner, ids[1], FileCheckState::Failed).await;
    assert_eq!(env.published("b.bin"), b"bravo");
    assert_eq!(env.server.requests("b.bin"), 1);
    // The checksum file itself has no check.
    assert_eq!(support::complete(&owner, ids[2]).await.check, None);

    // A retry removes the file that failed and downloads it again.
    env.server.serve("b.bin", "charlie");
    let retried = retry(&owner, ids[1]).await.unwrap();
    assert!(matches!(
        retried,
        CommandResult::JobUpdated { changed: true, .. }
    ));
    let again = checked(&owner, ids[1], FileCheckState::Verified).await;
    assert!(again.generation > b.generation);
    assert!(
        again
            .restart_reason
            .as_ref()
            .is_some_and(|r| r.as_str().contains("release.sfv")),
        "{again:?}"
    );
    assert_eq!(env.published("b.bin"), b"charlie");
    assert_eq!(again.artifact.unwrap().path, "b.bin");
    assert_eq!(env.server.requests("b.bin"), 2);
    // A complete file that passed is not retried.
    let refused = retry(&owner, ids[0]).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_file_is_downloaded_again_once_when_turned_on() {
    let env = env();
    env.server.serve("c.bin", "bad bytes");
    env.server
        .serve("c.md5", "0123456789abcdef0123456789abcdef  c.bin\n");
    let owner = env.start().await;
    redownload(&owner, true).await;
    let ids = submit(&owner, &["c.bin", "c.md5"]).await;
    let c = wait_for(&owner, ids[0], |j| {
        j.stage == JobStage::Complete
            && check(j) == Some(FileCheckState::Failed)
            && j.generation.get() > 1
    })
    .await;
    // Downloaded once more, from the start, and then left failed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.server.requests("c.bin"), 2);
    assert_eq!(
        check(&job(&owner, ids[0]).await),
        Some(FileCheckState::Failed)
    );
    assert_eq!(c.check.unwrap().algorithm, Algorithm::Md5);
    assert_eq!(env.staging_files(), 0);
    // The budget survives a restart.
    shutdown(owner).await;
    let owner = env.start().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.server.requests("c.bin"), 2);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn checksum_files_that_disagree_are_a_conflict_and_never_download_again() {
    let env = env();
    env.server.serve("d.bin", "delta");
    env.server
        .serve("one.sfv", format!("d.bin {}\n", crc(b"delta")));
    env.server.serve("two.sfv", "d.bin 00000000\n");
    let owner = env.start().await;
    redownload(&owner, true).await;
    let ids = submit(&owner, &["d.bin", "one.sfv", "two.sfv"]).await;
    let d = checked(&owner, ids[0], FileCheckState::Conflict).await;
    assert!(d.check.unwrap().problem.is_some());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(env.server.requests("d.bin"), 1);
    assert_eq!(env.published("d.bin"), b"delta");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_changed_outside_wayfetch_is_not_removed() {
    let env = env();
    env.server.serve("e.bin", "echo");
    env.server.serve("e.sfv", "e.bin 00000000\n");
    let owner = env.start().await;
    let ids = submit(&owner, &["e.bin", "e.sfv"]).await;
    checked(&owner, ids[0], FileCheckState::Failed).await;
    // Same size, other bytes, written later.
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(env.root.join("e.bin"), b"ECHO").unwrap();
    let refused = retry(&owner, ids[0]).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable, "{refused:?}");
    assert_eq!(env.published("e.bin"), b"ECHO");
    let e = checked(&owner, ids[0], FileCheckState::Changed).await;
    assert!(e.check.unwrap().problem.is_some());
    assert_eq!(env.server.requests("e.bin"), 1);
    // Retrying again says the same and changes nothing.
    let again = retry(&owner, ids[0]).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::NotApplicable);
    assert_eq!(env.published("e.bin"), b"ECHO");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archives_wait_for_their_checksum_file_and_parts_that_failed() {
    let mut env = env();
    let unpacker = Arc::<Unpacker>::default();
    env.extractor = Some(unpacker.clone());
    env.server.serve("Good.zip", "good zip");
    env.server.serve("Bad.zip", "bad zip");
    env.server.serve(
        "zips.sfv",
        format!("Good.zip {}\nBad.zip {}\n", crc(b"good zip"), crc(b"other")),
    );
    env.server.hold("zips.sfv", true);
    let owner = env.start().await;
    let ids = submit(&owner, &["Good.zip", "Bad.zip", "zips.sfv"]).await;
    let waiting = wait_for(&owner, ids[0], |j| {
        j.extraction.as_ref().map(|x| x.state) == Some(ExtractionState::Waiting)
    })
    .await;
    assert_eq!(
        waiting.extraction.unwrap().problem.unwrap().as_str(),
        "Waiting for the checksum file to download."
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(unpacker.started.lock().unwrap().is_empty());
    env.server.hold("zips.sfv", false);
    wait_for(&owner, ids[0], |j| {
        j.extraction.as_ref().map(|x| x.state) == Some(ExtractionState::Complete)
    })
    .await;
    let bad = wait_for(&owner, ids[1], |j| {
        j.extraction
            .as_ref()
            .and_then(|x| x.problem.as_ref())
            .is_some_and(|p| p.as_str().contains("failed its check"))
    })
    .await;
    assert_eq!(check(&bad), Some(FileCheckState::Failed));
    assert_eq!(*unpacker.started.lock().unwrap(), ["Good.zip"]);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_checksum_download_releases_its_archives() {
    let mut env = env();
    let unpacker = Arc::<Unpacker>::default();
    env.extractor = Some(unpacker.clone());
    env.server.hold("set.sfv", true);
    let owner = env.start().await;
    let ids = submit(&owner, &["Solo.zip", "set.sfv"]).await;
    wait_for(&owner, ids[0], |j| {
        j.extraction.as_ref().map(|x| x.state) == Some(ExtractionState::Waiting)
    })
    .await;
    let sfv = wait_for(&owner, ids[1], |j| j.stage == JobStage::Transferring).await;
    command(
        &owner,
        Command::Cancel {
            job: ids[1],
            expected_revision: sfv.revision,
        },
    )
    .await
    .unwrap();
    env.server.hold("set.sfv", false);
    let solo = wait_for(&owner, ids[0], |j| {
        j.extraction.as_ref().map(|x| x.state) == Some(ExtractionState::Complete)
    })
    .await;
    // Unpacked unchecked.
    assert_eq!(solo.check, None);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turned_off_nothing_is_checked() {
    let env = env();
    env.server.serve("f.sfv", "f.bin 00000000\n");
    let owner = env.start().await;
    let current = settings(&owner).await;
    command(
        &owner,
        Command::UpdateSettings {
            expected_revision: current.revision,
            default_root: None,
            max_active_downloads: None,
            download_speed_limit: None,
            extract_archives: None,
            automatic_update_checks: None,
            check_files: Some(false),
            redownload_failed_checks: None,
        },
    )
    .await
    .unwrap();
    let ids = submit(&owner, &["f.bin", "f.sfv"]).await;
    support::complete(&owner, ids[1]).await;
    let f = support::complete(&owner, ids[0]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.check, None);
    assert_eq!(job(&owner, ids[0]).await.check, None);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_replacement_finishes_after_a_restart() {
    let env = env();
    env.server.serve("g.bin", "golf");
    env.server
        .serve("g.sfv", format!("g.bin {}\n", crc(b"GOLF")));
    let owner = env.start().await;
    let ids = submit(&owner, &["g.bin", "g.sfv"]).await;
    let g = checked(&owner, ids[0], FileCheckState::Failed).await;
    shutdown(owner).await;
    // As if the engine stopped right after authorizing the replacement,
    // and again after removing the file, before finishing it.
    let db = rusqlite::Connection::open(env.state.join("state.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO replacements (job_id, publication_id, generation, reason, created_at) \
         SELECT job_id, id, generation, 'the downloaded file did not match', 0 \
         FROM publications WHERE job_id = ?1 AND state = 'published'",
        [g.id.get() as i64],
    )
    .unwrap();
    drop(db);
    std::fs::remove_file(env.root.join("g.bin")).unwrap();
    env.server.serve("g.bin", "GOLF");
    let owner = env.start().await;
    let again = checked(&owner, ids[0], FileCheckState::Verified).await;
    assert!(again.generation > g.generation);
    assert_eq!(env.published("g.bin"), b"GOLF");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_file_that_failed_is_still_downloaded_again() {
    let env = env();
    env.server.serve("h.bin", "hotel");
    env.server
        .serve("h.sfv", format!("h.bin {}\n", crc(b"india")));
    let owner = env.start().await;
    let ids = submit(&owner, &["h.bin", "h.sfv"]).await;
    let h = checked(&owner, ids[0], FileCheckState::Failed).await;
    let renamed = command(
        &owner,
        Command::Rename {
            job: ids[0],
            expected_revision: h.revision,
            name: "hotel.bin".into(),
        },
    )
    .await;
    assert!(renamed.is_ok(), "{renamed:?}");
    assert_eq!(env.published("hotel.bin"), b"hotel");
    // The check stays with the file under its new name.
    let h = checked(&owner, ids[0], FileCheckState::Failed).await;
    assert_eq!(h.artifact.unwrap().path, "hotel.bin");

    env.server.serve("h.bin", "india");
    retry(&owner, ids[0]).await.unwrap();
    // The renamed file is removed, and the new one takes the download's name.
    let again = checked(&owner, ids[0], FileCheckState::Verified).await;
    assert_eq!(again.artifact.unwrap().path, "h.bin");
    assert_eq!(env.published("h.bin"), b"india");
    assert!(!env.root.join("hotel.bin").exists());
    shutdown(owner).await;
}
