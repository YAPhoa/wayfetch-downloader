//! File hosts' links through the public engine API, with an in-process file
//! host that stores files encoded, and its storage server. MEGA itself is
//! tested in `wayfetch-adapters`.

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, KIB, body, complete, envelope, job, shutdown, submit, wait_for};
use url::Url;
use wayfetch_engine::EngineOwner;
use wayfetch_engine::hosting::{
    Decoder, Encoding, FileHost, HostCheck, HostError, HostedFile, Verifier,
};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_protocol::command::{CollectMode, CollectText};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::name::ProviderId;
use wayfetch_protocol::query::{CollectedLink, JobSummary, LinkCheck, PageSize};
use wayfetch_protocol::{Command, Query, QueryResult};

const SERVICE: &str = "synthetic-host";
const STORAGE: &str = "storage.host.example";
const FILE: &str = "https://host.example/file/abc#key";
const FOLDER: &str = "https://host.example/folder/xyz#key";
const NAME: &str = "Synthetic File.bin";

/// The encoding: each byte XORed with a mask that depends on its offset.
fn mask(offset: u64) -> u8 {
    (offset.wrapping_mul(7) % 251) as u8 ^ 0x5a
}

fn encode(plain: &[u8]) -> Bytes {
    plain
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask(i as u64))
        .collect()
}

struct Xor(u64);

impl Decoder for Xor {
    fn decode(&mut self, data: &mut [u8]) {
        for b in data {
            *b ^= mask(self.0);
            self.0 += 1;
        }
    }
}

/// Collects the decoded file and compares it with what the host stored.
struct Same {
    expected: Bytes,
    seen: Vec<u8>,
}

impl Verifier for Same {
    fn update(&mut self, data: &[u8]) {
        self.seen.extend_from_slice(data);
    }

    fn verify(self: Box<Self>) -> bool {
        self.seen == self.expected
    }
}

struct Stored {
    plain: Bytes,
}

impl Encoding for Stored {
    fn decoder(&self, offset: u64) -> Box<dyn Decoder> {
        Box::new(Xor(offset))
    }

    fn verifier(&self) -> Box<dyn Verifier> {
        Box::new(Same {
            expected: self.plain.clone(),
            seen: Vec::new(),
        })
    }
}

/// A storage request: its URL path and range.
type Request = (String, Option<(u64, Option<u64>)>);

/// The file host and its storage server.
struct Host {
    id: ProviderId,
    plain: Bytes,
    /// What storage serves: the encoded file, unless damaged.
    stored: Mutex<Bytes>,
    /// Serving stops before this byte until cleared.
    stall_at: Mutex<Option<u64>>,
    /// Storage refuses every request with this status.
    refuse: Mutex<Option<u16>>,
    resolves: AtomicUsize,
    /// Ranges storage was asked for, by URL path and query.
    requests: Mutex<Vec<Request>>,
    folder_files: Vec<Url>,
    /// For a host that stores files as they are, the `ETag` of the version
    /// it stores, if it has one; `None` for an encoding host.
    version: Option<Mutex<Option<String>>>,
}

impl Host {
    fn new(plain: Bytes) -> Arc<Self> {
        Self::build(encode(&plain), plain, None)
    }

    /// A host that stores files as they are, under validators only.
    fn unencoded(plain: Bytes, etag: Option<&str>) -> Arc<Self> {
        Self::build(
            plain.clone(),
            plain,
            Some(Mutex::new(etag.map(str::to_owned))),
        )
    }

    fn build(stored: Bytes, plain: Bytes, version: Option<Mutex<Option<String>>>) -> Arc<Self> {
        Arc::new(Self {
            id: ProviderId::new(SERVICE).unwrap(),
            stored: Mutex::new(stored),
            plain,
            version,
            stall_at: Mutex::new(None),
            refuse: Mutex::new(None),
            resolves: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            folder_files: ["one", "two"]
                .iter()
                .map(|f| Url::parse(&format!("{FOLDER}/file/{f}")).unwrap())
                .collect(),
        })
    }

    fn etag(&self) -> Option<String> {
        self.version.as_ref()?.lock().unwrap().clone()
    }

    /// Replaces the stored file with `plain`, as version `etag`.
    fn replace(&self, plain: Bytes, etag: Option<&str>) {
        *self.stored.lock().unwrap() = plain;
        *self.version.as_ref().unwrap().lock().unwrap() = etag.map(str::to_owned);
    }
}

impl FileHost for Host {
    fn service(&self) -> &ProviderId {
        &self.id
    }

    fn recognizes(&self, link: &Url) -> bool {
        link.host_str() == Some("host.example") && link.fragment().is_some()
    }

    fn check<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, HostCheck> {
        Box::pin(async move {
            if link.path().starts_with("/folder/") && !link.fragment().unwrap().contains('/') {
                HostCheck::Folder(self.folder_files.clone())
            } else {
                HostCheck::File {
                    name: Some(NAME.into()),
                    size: Some(self.plain.len() as u64),
                }
            }
        })
    }

    fn resolve<'a>(&'a self, _: &'a Url) -> BoxFuture<'a, Result<HostedFile, HostError>> {
        Box::pin(async move {
            // Every answer is a fresh URL.
            let n = self.resolves.fetch_add(1, Ordering::SeqCst);
            let unencoded = self.version.is_some();
            Ok(HostedFile {
                url: Url::parse(&format!("https://{STORAGE}/dl/{n}")).unwrap(),
                name: Some(NAME.into()),
                size: self.plain.len() as u64,
                identity: "abc;v1".into(),
                encoding: (!unencoded).then(|| {
                    Arc::new(Stored {
                        plain: self.plain.clone(),
                    }) as Arc<dyn Encoding>
                }),
                validators: unencoded.then(|| Representation {
                    strong_etag: self.etag(),
                    ..Representation::default()
                }),
            })
        })
    }

    fn refused(&self, status: u16) -> Option<HostError> {
        (status == 509).then(|| HostError::new(FailureCode::QuotaExhausted, "quota used up"))
    }
}

struct Body {
    host: Arc<Host>,
    data: Bytes,
    offset: u64,
    position: usize,
}

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
        Box::pin(async move {
            loop {
                let stall = *self.host.stall_at.lock().unwrap();
                let at = self.offset + self.position as u64;
                if stall.is_some_and(|limit| at >= limit) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                if self.position == self.data.len() {
                    return Ok(None);
                }
                let mut end = (self.position + KIB).min(self.data.len());
                if let Some(limit) = stall {
                    end = end.min(self.position + usize::try_from(limit - at).unwrap());
                }
                let chunk = self.data.slice(self.position..end);
                self.position = end;
                return Ok(Some(chunk));
            }
        })
    }
}

struct Storage(Arc<Host>);

impl Transport for Storage {
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        Box::pin(async move {
            let host = &self.0;
            assert_eq!(
                request.url.host_str(),
                Some(STORAGE),
                "only storage is asked"
            );
            host.requests
                .lock()
                .unwrap()
                .push((request.url.path().to_owned(), request.range));
            if let Some(status) = *host.refuse.lock().unwrap() {
                return Err(TransportError {
                    code: FailureCode::RequestTimeout,
                    message: format!("HTTP {status}"),
                    status: Some(status),
                });
            }
            let data = host.stored.lock().unwrap().clone();
            let len = data.len() as u64;
            let etag = host.etag();
            // `If-Range`: a range of another version is answered whole.
            let current = request
                .validators
                .as_ref()
                .is_none_or(|v| v.strong_etag.is_none() || v.strong_etag == etag);
            let (framing, start) = match request.range {
                Some((start, _)) if current => (
                    Framing::Partial {
                        first: start,
                        last: len - 1,
                        complete: Some(len),
                    },
                    start,
                ),
                _ => (Framing::Full { length: Some(len) }, 0),
            };
            Ok(Response {
                framing,
                representation: Representation {
                    length: Some(len),
                    strong_etag: etag,
                    ..Representation::default()
                },
                filename: None,
                body: Box::new(Body {
                    host: host.clone(),
                    data: data.slice(usize::try_from(start).unwrap()..),
                    offset: start,
                    position: 0,
                }),
            })
        })
    }
}

type Env = support::Env<Arc<Host>>;

fn env(plain: Bytes) -> Env {
    env_with(Host::new(plain))
}

fn env_with(host: Arc<Host>) -> Env {
    let mut env = Env::new(host.clone(), Arc::new(Storage(host.clone())));
    // No inspector: links are checked by the file host, never by their
    // own server.
    env.hosts.push(host);
    env
}

#[tokio::test]
async fn a_hosted_file_is_decoded_checked_and_published_under_its_name() {
    let plain = body(20 * KIB);
    let env = env(plain.clone());
    let owner = env.start().await;
    let id = submit(&owner, [FILE.to_owned()]).await[0];
    let done = complete(&owner, id).await;
    assert_eq!(done.bytes_total, Some(20 * KIB as u64));
    assert_eq!(env.published(NAME), plain);
    assert_eq!(env.server.requests.lock().unwrap().len(), 1);
    assert_eq!(env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test]
async fn a_paused_download_continues_from_a_fresh_url_decoding_from_its_offset() {
    let plain = body(24 * KIB);
    let env = env(plain.clone());
    *env.server.stall_at.lock().unwrap() = Some(10 * KIB as u64 + 7);
    let owner = env.start().await;
    let id = submit(&owner, [FILE.to_owned()]).await[0];
    let at = wait_for(&owner, id, |j| j.bytes_done >= 8 * KIB as u64).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: at.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let paused = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    let saved = paused.bytes_done;
    assert!(saved > 0, "{paused:?}");
    *env.server.stall_at.lock().unwrap() = None;
    let resume = Command::Resume {
        job: id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    let done = complete(&owner, id).await;
    assert_eq!(
        done.generation, paused.generation,
        "continued, not restarted"
    );
    assert_eq!(env.published(NAME), plain);
    let requests = env.server.requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        [
            ("/dl/0".into(), None),
            ("/dl/1".into(), Some((saved, None)))
        ]
    );
    shutdown(owner).await;
}

#[tokio::test]
async fn a_file_that_fails_its_check_is_not_published() {
    let plain = body(12 * KIB);
    let env = env(plain.clone());
    // Storage serves one damaged byte.
    {
        let mut stored = env.server.stored.lock().unwrap();
        let mut damaged = stored.to_vec();
        damaged[5000] ^= 1;
        *stored = damaged.into();
    }
    let owner = env.start().await;
    let id = submit(&owner, [FILE.to_owned()]).await[0];
    let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    let failure = failed.failure.unwrap();
    assert_eq!(failure.code, FailureCode::IntegrityMismatch);
    assert!(!env.root.join(NAME).exists());
    // Downloaded whole and still wrong: reported, not tried again.
    assert_eq!(env.server.resolves.load(Ordering::SeqCst), 1);
    shutdown(owner).await;
}

#[tokio::test]
async fn a_refusal_the_host_explains_fails_with_its_meaning() {
    let env = env(body(4 * KIB));
    *env.server.refuse.lock().unwrap() = Some(509);
    let owner = env.start().await;
    let id = submit(&owner, [FILE.to_owned()]).await[0];
    let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    let failure = failed.failure.unwrap();
    assert_eq!(failure.code, FailureCode::QuotaExhausted);
    assert_eq!(failure.message.unwrap().as_str(), "quota used up");
    let summary = job(&owner, id).await;
    assert_eq!(summary.stage, JobStage::Failed);
    shutdown(owner).await;
}

async fn listed(owner: &EngineOwner) -> Vec<CollectedLink> {
    let query = Query::ListLinks {
        after: None,
        page_size: PageSize::DEFAULT,
    };
    match owner.handle().query(query, ALL).await.unwrap() {
        QueryResult::Links(page) => page.items,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_folder_lists_its_files_on_the_links_list_and_each_is_checked_by_the_host() {
    let env = env(body(KIB));
    let owner = env.start().await;
    // A file host downloads without an account, so a filter leaving out
    // account links keeps its links.
    let revision = match owner.handle().query(Query::Settings {}, ALL).await.unwrap() {
        QueryResult::Settings(settings) => settings.revision,
        other => panic!("{other:?}"),
    };
    let filters = serde_json::from_value(serde_json::json!({
        "enabled": true,
        "filters": [{
            "name": "Account links",
            "enabled": true,
            "route": { "op": "is", "values": ["account"] },
        }],
    }))
    .unwrap();
    let settings = Command::UpdateSettings {
        expected_revision: revision,
        default_root: None,
        max_active_downloads: None,
        download_speed_limit: None,
        extract_archives: None,
        automatic_update_checks: None,
        link_filters: Some(filters),
    };
    owner
        .handle()
        .execute(envelope(&owner, settings), ALL)
        .await
        .unwrap();
    let command = Command::CollectLinks {
        text: CollectText::try_from(FOLDER.to_owned()).unwrap(),
        base_url: None,
        mode: CollectMode::SupportedOnly,
        destination: None,
    };
    owner
        .handle()
        .execute(envelope(&owner, command), ALL)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let links = loop {
        let links = listed(&owner).await;
        if links.len() == 3 && links.iter().all(|l| l.check != LinkCheck::Checking) {
            break links;
        }
        assert!(tokio::time::Instant::now() < deadline, "{links:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let via = ProviderId::new(SERVICE).unwrap();
    let folder = links.iter().find(|l| l.found.is_some()).unwrap();
    assert_eq!((folder.check, folder.found), (LinkCheck::Page, Some(2)));
    let files: HashMap<_, _> = links
        .iter()
        .filter(|l| l.found.is_none())
        .map(|l| (l.id, l))
        .collect();
    assert_eq!(files.len(), 2);
    for file in files.values() {
        assert_eq!(file.check, LinkCheck::Online);
        assert_eq!(file.size, Some(KIB as u64));
        assert_eq!(file.checked_via.as_ref(), Some(&via));
    }
    shutdown(owner).await;
}

#[tokio::test]
async fn continued_bytes_that_fail_the_check_are_downloaded_again_from_zero() {
    let plain = body(16 * KIB);
    let env = env(plain.clone());
    let good = env.server.stored.lock().unwrap().clone();
    // The first attempt saves a damaged byte, then stalls.
    let mut damaged = good.to_vec();
    damaged[100] ^= 1;
    *env.server.stored.lock().unwrap() = damaged.into();
    *env.server.stall_at.lock().unwrap() = Some(8 * KIB as u64);
    let owner = env.start().await;
    let id = submit(&owner, [FILE.to_owned()]).await[0];
    let at = wait_for(&owner, id, |j| j.bytes_done >= 4 * KIB as u64).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: at.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let paused = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    *env.server.stored.lock().unwrap() = good;
    *env.server.stall_at.lock().unwrap() = None;
    let resume = Command::Resume {
        job: id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    let done = complete(&owner, id).await;
    assert!(done.generation > paused.generation, "restarted from zero");
    assert!(done.restart_reason.is_some(), "{done:?}");
    assert_eq!(env.published(NAME), plain);
    let requests = env.server.requests.lock().unwrap().clone();
    assert_eq!(
        requests.last().unwrap().1,
        None,
        "the last request is whole"
    );
    shutdown(owner).await;
}

/// Starts `env`, pauses its download of [`FILE`] once some bytes are
/// saved, and returns the paused job.
async fn paused_partway(env: &Env) -> (EngineOwner, JobSummary) {
    *env.server.stall_at.lock().unwrap() = Some(8 * KIB as u64);
    let owner = env.start().await;
    let id = submit(&owner, [FILE.to_owned()]).await[0];
    let at = wait_for(&owner, id, |j| j.bytes_done >= 4 * KIB as u64).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: at.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let paused = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    assert!(paused.bytes_done > 0, "{paused:?}");
    *env.server.stall_at.lock().unwrap() = None;
    (owner, paused)
}

async fn resume(owner: &EngineOwner, paused: &JobSummary) {
    let resume = Command::Resume {
        job: paused.id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(owner, resume), ALL)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_unencoded_file_continues_under_the_same_validators() {
    let plain = body(16 * KIB);
    let env = env_with(Host::unencoded(plain.clone(), Some("\"v1\"")));
    let (owner, paused) = paused_partway(&env).await;
    resume(&owner, &paused).await;
    let done = complete(&owner, paused.id).await;
    assert_eq!(done.generation, paused.generation, "continued");
    assert_eq!(env.published(NAME), plain);
    shutdown(owner).await;
}

#[tokio::test]
async fn a_file_replaced_behind_the_same_link_is_downloaded_again_from_zero() {
    let plain = body(16 * KIB);
    let env = env_with(Host::unencoded(plain.clone(), Some("\"v1\"")));
    let (owner, paused) = paused_partway(&env).await;
    // Another file of the same size, under the same host identity.
    let other: Bytes = plain.iter().map(|b| b ^ 0xff).collect();
    env.server.replace(other.clone(), Some("\"v2\""));
    resume(&owner, &paused).await;
    let done = complete(&owner, paused.id).await;
    assert!(done.generation > paused.generation, "restarted from zero");
    assert_eq!(env.published(NAME), other);
    shutdown(owner).await;
}

#[tokio::test]
async fn a_file_without_validators_is_never_continued() {
    let plain = body(16 * KIB);
    let env = env_with(Host::unencoded(plain.clone(), None));
    let (owner, paused) = paused_partway(&env).await;
    let other: Bytes = plain.iter().map(|b| b ^ 0xff).collect();
    env.server.replace(other.clone(), None);
    resume(&owner, &paused).await;
    let done = complete(&owner, paused.id).await;
    assert!(done.generation > paused.generation, "restarted from zero");
    assert_eq!(env.published(NAME), other);
    let requests = env.server.requests.lock().unwrap().clone();
    assert!(
        requests.iter().all(|(_, range)| range.is_none()),
        "{requests:?}"
    );
    shutdown(owner).await;
}
