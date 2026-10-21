//! Unpacking downloaded archives through the public engine API, with an
//! in-process extractor (ADR-18, ADR-19). 7-Zip itself is tested in
//! `wayfetch-adapters`.

mod support;

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, envelope, job, shutdown, wait_for};
use wayfetch_engine::EngineOwner;
use wayfetch_engine::extract::{Archive, Entry, EntryKind, ExtractError, Extractor, Halt};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_protocol::domain::id::JobId;
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::media::{PluginRole, PluginState, PluginSummary};
use wayfetch_protocol::query::{ExtractionState, JobSummary, Settings};
use wayfetch_protocol::request::ExtractRequest;
use wayfetch_protocol::{
    Command, CommandResult, ErrorCode, IdempotencyKey, ProtocolError, Query, QueryResult,
    StreamItem, Subscribe,
};

/// Files served at every path; a held path's body does not end until it
/// is released.
#[derive(Default)]
struct Files {
    held: Mutex<HashSet<String>>,
}

impl Files {
    fn hold(&self, path: &str, held: bool) {
        let mut paths = self.held.lock().unwrap();
        if held {
            paths.insert(path.to_owned());
        } else {
            paths.remove(path);
        }
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
            let data = Bytes::from(format!("archive at {path}"));
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

/// The extractor: what each archive holds and does, by its file name.
#[derive(Default)]
struct Fake {
    /// Unpacking `slow.zip` waits for this.
    release: AtomicBool,
    /// Attempts to unpack, by archive name.
    started: Mutex<Vec<String>>,
    /// Whether the plugin is turned off.
    off: AtomicBool,
    /// Listings asked for.
    listings: AtomicUsize,
    /// The other volumes each attempt was given, by file name.
    volumes: Mutex<Vec<Vec<String>>>,
}

const PASSWORD: &str = "open sesame";
const WRONG: &str = "not the password";

fn file(path: &str, size: u64) -> Entry {
    Entry {
        path: path.into(),
        kind: EntryKind::File,
        size: Some(size),
        encrypted: false,
    }
}

fn folder(path: &str) -> Entry {
    Entry {
        path: path.into(),
        kind: EntryKind::Directory,
        size: None,
        encrypted: false,
    }
}

fn name(archive: &Archive<'_>) -> String {
    archive
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn password(archive: &Archive<'_>) -> Option<String> {
    archive
        .password
        .map(|p| p.expose_text().unwrap().to_owned())
}

impl Extractor for Fake {
    fn plugin(&self) -> PluginSummary {
        PluginSummary {
            id: "fake-unpacker".into(),
            name: "Fake unpacker".into(),
            role: PluginRole::ArchiveExtractor,
            services: Vec::new(),
            state: PluginState::Ready,
            version: Some("1.0".into()),
            problem: None,
            enabled: !self.off.load(Ordering::SeqCst),
            updatable: false,
            installed: false,
            fallback_version: None,
        }
    }

    fn list<'a>(
        &'a self,
        archive: Archive<'a>,
        _halt: Halt,
    ) -> BoxFuture<'a, Result<Vec<Entry>, ExtractError>> {
        Box::pin(async move {
            self.listings.fetch_add(1, Ordering::SeqCst);
            Ok(match name(&archive).as_str() {
                "one.zip" => vec![file("only.txt", 3)],
                "evil.zip" => vec![file("ok.txt", 1), file("../escape.txt", 1)],
                "linked.zip" => vec![Entry {
                    kind: EntryKind::Link,
                    ..file("link", 5)
                }],
                "secret.7z" => vec![Entry {
                    encrypted: true,
                    ..file("plans.txt", 6)
                }],
                "bomb.zip" => vec![file("small.txt", 10)],
                _ => vec![file("a.txt", 5), folder("d"), file("d/b.txt", 5)],
            })
        })
    }

    fn extract<'a>(
        &'a self,
        archive: Archive<'a>,
        output: &'a Path,
        _budget: u64,
        halt: Halt,
    ) -> BoxFuture<'a, Result<(), ExtractError>> {
        let unpack = async move {
            let name = name(&archive);
            self.started.lock().unwrap().push(name.clone());
            self.volumes.lock().unwrap().push(
                archive
                    .volumes
                    .iter()
                    .map(|v| v.file_name().unwrap().to_string_lossy().into_owned())
                    .collect(),
            );
            let write = |path: &str, data: &[u8]| {
                let path = output.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, data).unwrap();
            };
            match name.as_str() {
                "one.zip" => write("only.txt", b"one"),
                "secret.7z" => {
                    if password(&archive).as_deref() != Some(PASSWORD) {
                        return Err(ExtractError::WrongPassword);
                    }
                    write("plans.txt", b"secret");
                }
                "liar.zip" => {
                    write("a.txt", b"hello");
                    std::os::unix::fs::symlink("/etc/passwd", output.join("d")).unwrap();
                }
                "bomb.zip" => {
                    // Far more than the 10 bytes listed, written slowly
                    // enough for the watch to see it.
                    for i in 0..400 {
                        write(&format!("part{i}"), &[0; 64 * 1024]);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                slow if slow.starts_with("slow.") => {
                    while !self.release.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    write("a.txt", b"hello");
                }
                "damaged.zip" => {
                    return Err(ExtractError::Damaged("The archive is damaged.".into()));
                }
                _ => {
                    write("a.txt", b"hello");
                    write("d/b.txt", b"world");
                }
            }
            Ok(())
        };
        Box::pin(async move {
            tokio::select! {
                done = unpack => done,
                () = halt => Err(ExtractError::Stopped),
            }
        })
    }
}

type Env = support::Env<Arc<Files>>;

fn env() -> (Env, Arc<Fake>) {
    let files = Arc::<Files>::default();
    let mut env = Env::new(files.clone(), Arc::new(Served(files)));
    let fake = Arc::<Fake>::default();
    env.extractor = Some(fake.clone());
    (env, fake)
}

async fn submit(owner: &EngineOwner, names: &[&str]) -> Vec<JobId> {
    support::submit(
        owner,
        names.iter().map(|n| format!("https://files.example/{n}")),
    )
    .await
}

fn state(job: &JobSummary) -> Option<ExtractionState> {
    job.extraction.as_ref().map(|x| x.state)
}

/// Waits until `id`'s unpacking reaches `wanted`.
async fn unpacking(owner: &EngineOwner, id: JobId, wanted: ExtractionState) -> JobSummary {
    wait_for(owner, id, |j| state(j) == Some(wanted)).await
}

fn problem(job: &JobSummary) -> Option<&str> {
    job.extraction
        .as_ref()
        .and_then(|x| x.problem.as_ref())
        .map(|p| p.as_str())
}

fn output(job: &JobSummary) -> Option<&str> {
    job.extraction.as_ref().and_then(|x| x.output.as_deref())
}

async fn extract(
    owner: &EngineOwner,
    id: JobId,
    password: Option<&str>,
) -> Result<CommandResult, ProtocolError> {
    let current = job(owner, id).await;
    let request = ExtractRequest {
        idempotency_key: IdempotencyKey::generate(),
        target: owner.identity().instance,
        expected_revision: current.revision,
        expected_generation: current.extraction.and_then(|x| x.generation),
        password: password.map(str::to_owned),
    };
    owner.handle().extract(id, request, ALL).await
}

async fn command(owner: &EngineOwner, command: Command) -> Result<CommandResult, ProtocolError> {
    owner.handle().execute(envelope(owner, command), ALL).await
}

async fn settings(owner: &EngineOwner) -> Settings {
    match owner.handle().query(Query::Settings {}, ALL).await.unwrap() {
        QueryResult::Settings(settings) => settings,
        other => panic!("{other:?}"),
    }
}

/// What the root holds, other than Wayfetch's private folder.
fn listing(root: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".wayfetch")
        .collect();
    names.sort_unstable();
    names
}

/// Private extraction folders left in the root.
fn private_folders(root: &Path) -> usize {
    std::fs::read_dir(root.join(".wayfetch/extract"))
        .map(|profiles| {
            profiles
                .map(|p| std::fs::read_dir(p.unwrap().path()).unwrap().count())
                .sum()
        })
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_archive_unpacks_beside_itself_and_is_kept() {
    let (env, _fake) = env();
    let owner = env.start().await;
    let ids = submit(&owner, &["Pack.zip", "one.zip", "movie.mkv"]).await;
    let pack = unpacking(&owner, ids[0], ExtractionState::Complete).await;
    assert_eq!(output(&pack), Some("Pack"));
    let root = &env.root;
    assert_eq!(std::fs::read(root.join("Pack/a.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(root.join("Pack/d/b.txt")).unwrap(), b"world");
    // A single entry keeps its own name rather than a folder of its own.
    let one = unpacking(&owner, ids[1], ExtractionState::Complete).await;
    assert_eq!(output(&one), Some("only.txt"));
    assert_eq!(std::fs::read(root.join("only.txt")).unwrap(), b"one");
    // Something that is not an archive has no unpacking at all.
    let movie = support::complete(&owner, ids[2]).await;
    assert_eq!(movie.extraction, None);
    // Archives stay; nothing private is left.
    assert_eq!(
        listing(root),
        ["Pack", "Pack.zip", "movie.mkv", "one.zip", "only.txt"]
    );
    assert_eq!(private_folders(root), 0);

    // Unpacking again never merges into or replaces the first output.
    extract(&owner, ids[0], None).await.unwrap();
    let again = wait_for(&owner, ids[0], |j| output(j) == Some("Pack (2)")).await;
    assert_eq!(state(&again), Some(ExtractionState::Complete));
    assert!(root.join("Pack (2)/d/b.txt").is_file());

    // A file being unpacked cannot be renamed or deleted, but can once
    // it ends.
    let renamed = command(
        &owner,
        Command::Rename {
            job: ids[0],
            expected_revision: again.revision,
            name: "Renamed.zip".into(),
        },
    )
    .await;
    assert!(renamed.is_ok(), "{renamed:?}");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turned_off_archives_stay_packed_until_asked() {
    let (env, fake) = env();
    let owner = env.start().await;
    let current = settings(&owner).await;
    assert!(current.extract_archives);
    command(
        &owner,
        Command::UpdateSettings {
            expected_revision: current.revision,
            default_root: None,
            max_active_downloads: None,
            download_speed_limit: None,
            extract_archives: Some(false),
            automatic_updates: None,
        },
    )
    .await
    .unwrap();
    assert!(!settings(&owner).await.extract_archives);
    let handle = owner.handle();
    let head = handle.hello().await.unwrap().head;
    let mut events = handle
        .subscribe(Subscribe { after: Some(head) }, ALL)
        .await
        .unwrap();
    let ids = submit(&owner, &["Later.zip"]).await;
    let done = support::complete(&owner, ids[0]).await;
    assert_eq!(state(&done), Some(ExtractionState::Idle));
    // The event of its completion says so too, so a client can offer it.
    loop {
        let item = events.next().await.expect("stream ended early");
        if let StreamItem::Event(event) = item
            && let EventKind::JobChanged { job } = event.kind
            && job.stage == JobStage::Complete
        {
            assert_eq!(state(&job), Some(ExtractionState::Idle));
            break;
        }
    }
    assert!(fake.started.lock().unwrap().is_empty());
    // The setting survives a restart.
    shutdown(owner).await;
    let owner = env.start().await;
    assert!(!settings(&owner).await.extract_archives);
    // Asked for, it unpacks.
    extract(&owner, ids[0], None).await.unwrap();
    unpacking(&owner, ids[0], ExtractionState::Complete).await;
    assert!(env.root.join("Later/a.txt").is_file());

    // With the plugin turned off, unpacking is refused, and new archives
    // are not queued for it.
    fake.off.store(true, Ordering::SeqCst);
    let refused = extract(&owner, ids[0], None).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotApplicable);
    assert!(
        refused.message.as_str().contains("Fake unpacker"),
        "{refused:?}"
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_part_archive_waits_for_its_last_part() {
    let (env, fake) = env();
    env.server.hold("/Set.part2.rar", true);
    let owner = env.start().await;
    let ids = submit(&owner, &["Set.part1.rar", "Set.part2.rar"]).await;
    let first = unpacking(&owner, ids[0], ExtractionState::Waiting).await;
    assert_eq!(
        problem(&first),
        Some("Waiting for 1 more part to download.")
    );
    // Only the first part shows unpacking.
    assert_eq!(state(&job(&owner, ids[1]).await), None);
    assert!(fake.started.lock().unwrap().is_empty());
    env.server.hold("/Set.part2.rar", false);
    let done = unpacking(&owner, ids[0], ExtractionState::Complete).await;
    assert_eq!(output(&done), Some("Set"));
    assert_eq!(*fake.started.lock().unwrap(), ["Set.part1.rar"]);
    // The extractor is given the other part, and nothing else.
    assert_eq!(*fake.volumes.lock().unwrap(), [["Set.part2.rar"]]);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn every_part_of_an_archive_is_kept_while_it_unpacks() {
    let (env, fake) = env();
    let owner = env.start().await;
    let ids = submit(
        &owner,
        &["slow.part1.rar", "slow.part2.rar", "other.part2.rar"],
    )
    .await;
    unpacking(&owner, ids[0], ExtractionState::Extracting).await;
    let part = support::complete(&owner, ids[1]).await;
    for change in [
        Command::DeleteFiles {
            job: ids[1],
            expected_revision: part.revision,
        },
        Command::Rename {
            job: ids[1],
            expected_revision: part.revision,
            name: "renamed.part2.rar".into(),
        },
    ] {
        let busy = command(&owner, change).await.unwrap_err();
        assert_eq!(busy.code, ErrorCode::Busy, "{busy:?}");
    }
    // A part of another archive is not held.
    let other = support::complete(&owner, ids[2]).await;
    command(
        &owner,
        Command::Rename {
            job: ids[2],
            expected_revision: other.revision,
            name: "another.part2.rar".into(),
        },
    )
    .await
    .unwrap();

    fake.release.store(true, Ordering::SeqCst);
    unpacking(&owner, ids[0], ExtractionState::Complete).await;
    assert_eq!(*fake.volumes.lock().unwrap(), [["slow.part2.rar"]]);
    command(
        &owner,
        Command::DeleteFiles {
            job: ids[1],
            expected_revision: part.revision,
        },
    )
    .await
    .unwrap();
    assert!(!env.root.join("slow.part2.rar").exists());
    assert!(env.root.join("slow.part1.rar").is_file());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_protected_archive_asks_for_its_password() {
    let (env, fake) = env();
    let owner = env.start().await;
    let ids = submit(&owner, &["secret.7z"]).await;
    let asked = unpacking(&owner, ids[0], ExtractionState::NeedsPassword).await;
    assert_eq!(problem(&asked), None);
    // The listing said so; nothing was unpacked without a password.
    assert!(fake.started.lock().unwrap().is_empty());

    extract(&owner, ids[0], Some(WRONG)).await.unwrap();
    let wrong = wait_for(&owner, ids[0], |j| problem(j).is_some()).await;
    assert_eq!(state(&wrong), Some(ExtractionState::NeedsPassword));
    assert_eq!(problem(&wrong), Some("The password is wrong."));

    // An empty password is refused before it reaches the engine.
    let empty = extract(&owner, ids[0], Some("")).await.unwrap_err();
    assert_eq!(empty.code, ErrorCode::InvalidRequest);

    extract(&owner, ids[0], Some(PASSWORD)).await.unwrap();
    let done = unpacking(&owner, ids[0], ExtractionState::Complete).await;
    assert_eq!(output(&done), Some("plans.txt"));
    assert_eq!(
        std::fs::read(env.root.join("plans.txt")).unwrap(),
        b"secret"
    );
    // A password is used once: unpacking again asks for it again.
    extract(&owner, ids[0], None).await.unwrap();
    unpacking(&owner, ids[0], ExtractionState::NeedsPassword).await;
    shutdown(owner).await;

    // Passwords are never stored.
    let mut stored = Vec::new();
    for entry in walk(&env.state) {
        stored.extend(std::fs::read(entry).unwrap());
    }
    assert!(!contains(&stored, PASSWORD.as_bytes()));
    assert!(!contains(&stored, WRONG.as_bytes()));
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_or_broken_archives_leave_nothing_beside_them() {
    let (env, fake) = env();
    let owner = env.start().await;
    let names = [
        "evil.zip",
        "linked.zip",
        "liar.zip",
        "bomb.zip",
        "damaged.zip",
    ];
    let ids = submit(&owner, &names).await;
    let mut problems = Vec::new();
    for id in &ids {
        let failed = unpacking(&owner, *id, ExtractionState::Failed).await;
        problems.push(problem(&failed).unwrap().to_owned());
    }
    assert_eq!(
        problems,
        [
            "The archive names a file outside its own folder.",
            "The archive holds links, which are not unpacked.",
            "The archive holds links, which are not unpacked.",
            "The archive unpacks to far more than it should; it may be a decompression bomb.",
            "The archive is damaged.",
        ]
    );
    // The listing refused the first two before anything was unpacked;
    // downloads finish, and so are unpacked, in any order.
    let mut started = fake.started.lock().unwrap().clone();
    started.sort_unstable();
    assert_eq!(started, ["bomb.zip", "damaged.zip", "liar.zip"]);
    // Only the archives remain, and no private folder.
    let mut kept: Vec<_> = names.iter().map(|n| n.to_string()).collect();
    kept.sort_unstable();
    assert_eq!(listing(&env.root), kept);
    assert_eq!(private_folders(&env.root), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_and_restarting_keep_the_archive_and_try_again() {
    let (env, fake) = env();
    let owner = env.start().await;
    let ids = submit(&owner, &["slow.zip"]).await;
    let running = unpacking(&owner, ids[0], ExtractionState::Extracting).await;
    assert_eq!(private_folders(&env.root), 1);

    // Its file cannot change underneath it.
    let busy = command(
        &owner,
        Command::Rename {
            job: ids[0],
            expected_revision: running.revision,
            name: "other.zip".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(busy.code, ErrorCode::Busy);

    let cancel = Command::CancelExtraction {
        job: ids[0],
        expected_revision: running.revision,
        expected_generation: running.extraction.as_ref().unwrap().generation.unwrap(),
    };
    let sent = envelope(&owner, cancel.clone());
    owner.handle().execute(sent.clone(), ALL).await.unwrap();
    let cancelled = unpacking(&owner, ids[0], ExtractionState::Cancelled).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while private_folders(&env.root) > 0 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(listing(&env.root), ["slow.zip"]);

    // Started again, it is a new run: the earlier stop, sent again, does
    // not stop it, and neither request can start yet another run.
    extract(&owner, ids[0], None).await.unwrap();
    let again = unpacking(&owner, ids[0], ExtractionState::Extracting).await;
    let late = command(&owner, cancel).await.unwrap_err();
    assert_eq!(late.code, ErrorCode::RevisionConflict);
    // The very same request, sent again, is answered as before but stops
    // nothing.
    owner.handle().execute(sent, ALL).await.unwrap();
    let stale = ExtractRequest {
        idempotency_key: IdempotencyKey::generate(),
        target: owner.identity().instance,
        expected_revision: cancelled.revision,
        expected_generation: cancelled.extraction.and_then(|x| x.generation),
        password: Some(PASSWORD.into()),
    };
    let stale = owner
        .handle()
        .extract(ids[0], stale, ALL)
        .await
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::RevisionConflict);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let still = job(&owner, ids[0]).await;
    assert_eq!(state(&still), Some(ExtractionState::Extracting));
    assert_eq!(still.extraction, again.extraction);
    assert_eq!(fake.started.lock().unwrap().len(), 2);

    // Interrupted by a shutdown, it is unpacked again at the next start.
    shutdown(owner).await;
    assert_eq!(private_folders(&env.root), 1);
    fake.release.store(true, Ordering::SeqCst);
    let owner = env.start().await;
    let done = unpacking(&owner, ids[0], ExtractionState::Complete).await;
    assert_eq!(output(&done), Some("a.txt"));
    assert_eq!(listing(&env.root), ["a.txt", "slow.zip"]);
    assert_eq!(private_folders(&env.root), 0);
    assert_eq!(fake.started.lock().unwrap().len(), 3);
    shutdown(owner).await;
}
