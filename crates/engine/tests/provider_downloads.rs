//! Downloads through a provider account, through the public engine API,
//! with an in-process provider and download host (architecture brief
//! sections 6.5 and 7.3, ADR-09, ADR-24; test T12): the link is generated
//! only once a download starts, never while collecting or listing; the
//! provider's request is recorded before it is sent and never repeated
//! blindly; credentials stay current across concurrent calls; and a
//! cancelled, disabled, or removed account's work is never published.
//! Real-Debrid's own client is tested in `wayfetch-adapters`.

mod support;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, KIB, body, envelope, shutdown, wait_for};
use tokio::sync::Semaphore;
use url::Url;
use wayfetch_engine::EngineOwner;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, Checked, DeviceAuthorization, GeneratedLink, PollOutcome, Profile,
    ProviderError, Reconciled, Unrestricted, WithCredentials,
};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_protocol::account::{
    AccountAlias, AccountPlan, AccountRef, AuthorizationMethod, AuthorizationState, ServiceSupport,
};
use wayfetch_protocol::command::{CollectItems, LinkRef, LinkRefs};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::{AccountId, JobId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::name::ProviderId;
use wayfetch_protocol::domain::time::UtcTimestamp;
use wayfetch_protocol::query::{CollectedLink, JobRoute, JobSummary, PageSize};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, Permission, ProtocolError, Query,
    QueryResult,
};

const SERVICE: &str = "synthetic-debrid";
/// A hoster link, as collected.
const ORIGINAL: &str = "https://hoster.example/file/abc123";
/// What a generated link carries that grants access.
const SIGNATURE: &str = "synthetic-signature";
/// What the provider's credentials are named after.
const CREDENTIAL: &str = "synthetic-rotating-credential";
const CDN: &str = "cdn.synthetic.example";

/// A requested byte range: its first byte and, if bounded, its last.
type Range = (u64, Option<u64>);

/// How the provider answers the next link request.
#[derive(Debug, Clone)]
enum Answer {
    /// Generates a link and answers with it.
    Generate,
    /// Generates a link, then loses the answer.
    GenerateThenLose,
    /// Loses the request before generating anything.
    LoseRequest,
    /// Refuses.
    Refuse(FailureCode, &'static str, Option<Duration>),
    /// Generates a link, then waits until released before answering.
    GenerateThenWait,
    /// Waits until released, then generates and answers.
    WaitThenGenerate,
}

/// A provider that rotates its credential on every call when asked to,
/// and counts every call.
struct Provider {
    service: ProviderId,
    /// The number of the only credential the provider accepts now.
    current: Mutex<u64>,
    rotate: Mutex<bool>,
    /// Calls that presented an outdated credential.
    stale: AtomicUsize,
    unrestricts: AtomicUsize,
    lookups: AtomicUsize,
    checks: AtomicUsize,
    /// Calls running at once, and the most seen.
    running: AtomicUsize,
    most_running: AtomicUsize,
    answers: Mutex<VecDeque<Answer>>,
    /// Links generated, by original link: the provider's own record.
    record: Mutex<Vec<(String, GeneratedLink)>>,
    /// Released permits let waiting answers go on.
    gate: Semaphore,
    approvals: Semaphore,
    /// Delay of each call, so concurrent calls would overlap.
    delay: Mutex<Duration>,
    /// What the generated file is called.
    filename: Mutex<Option<String>>,
    size: Mutex<Option<u64>>,
}

impl Provider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            service: ProviderId::new(SERVICE).unwrap(),
            current: Mutex::new(0),
            rotate: Mutex::new(false),
            stale: AtomicUsize::new(0),
            unrestricts: AtomicUsize::new(0),
            lookups: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            most_running: AtomicUsize::new(0),
            answers: Mutex::new(VecDeque::new()),
            record: Mutex::new(Vec::new()),
            gate: Semaphore::new(0),
            approvals: Semaphore::new(0),
            delay: Mutex::new(Duration::ZERO),
            filename: Mutex::new(Some("Provider Name.bin".into())),
            size: Mutex::new(None),
        })
    }

    fn answer(&self, answer: Answer) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn unrestricts(&self) -> usize {
        self.unrestricts.load(Ordering::SeqCst)
    }

    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }

    fn credential(n: u64) -> Secret {
        Secret::from_text(&format!("{CREDENTIAL}-{n}"))
    }

    /// Accepts `credential` if current, rotating it when asked to. Returns
    /// the refreshed credential, or `None` with the call refused.
    async fn present(&self, credential: &Secret) -> Result<Option<Secret>, ()> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_running.fetch_max(now, Ordering::SeqCst);
        let delay = *self.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        let mut current = self.current.lock().unwrap();
        let presented = credential.expose_text().unwrap().to_owned();
        if presented != format!("{CREDENTIAL}-{current}") {
            self.stale.fetch_add(1, Ordering::SeqCst);
            return Err(());
        }
        if *self.rotate.lock().unwrap() {
            *current += 1;
            return Ok(Some(Self::credential(*current)));
        }
        Ok(None)
    }

    fn generate(&self, original: &Url) -> GeneratedLink {
        let n = self.record.lock().unwrap().len() + 1;
        let link = GeneratedLink {
            url: Url::parse(&format!("https://{CDN}/d/{n}/file.bin?sig={SIGNATURE}-{n}")).unwrap(),
            filename: self.filename.lock().unwrap().clone(),
            size: *self.size.lock().unwrap(),
        };
        self.record
            .lock()
            .unwrap()
            .push((original.as_str().to_owned(), link.clone()));
        link
    }
}

impl AccountProvider for Provider {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn support(&self) -> ServiceSupport {
        ServiceSupport {
            service: self.service.clone(),
            method: AuthorizationMethod::DeviceCode,
            downloads: true,
        }
    }

    fn begin(&self) -> BoxFuture<'_, Result<DeviceAuthorization, ProviderError>> {
        Box::pin(async move {
            Ok(DeviceAuthorization {
                user_code: "ABCD-1234".into(),
                verification_url: Url::parse("https://provider.example/device").unwrap(),
                expires_in: Duration::from_secs(600),
                interval: Duration::from_secs(1),
                pending: Secret::from_text("device"),
            })
        })
    }

    fn poll<'a>(&'a self, _: &'a Secret) -> BoxFuture<'a, Result<PollOutcome, ProviderError>> {
        Box::pin(async move {
            self.approvals.acquire().await.unwrap().forget();
            let n = {
                let mut current = self.current.lock().unwrap();
                *current += 1;
                *current
            };
            Ok(PollOutcome::Authorized {
                credential: Self::credential(n),
                profile: Profile {
                    // The same user each time, so it can sign in again.
                    user_id: Some("user-1".into()),
                    username: Some("synthetic-user".into()),
                    plan: Some(AccountPlan::Premium),
                    expires_at: None,
                },
            })
        })
    }

    fn check<'a>(&'a self, credential: &'a Secret) -> BoxFuture<'a, Checked> {
        Box::pin(async move {
            self.checks.fetch_add(1, Ordering::SeqCst);
            match self.present(credential).await {
                Err(()) => Checked::failed(ProviderError::InvalidCredentials("stale".into())),
                Ok(refreshed) => Checked {
                    result: Ok(Profile {
                        user_id: Some("user-1".into()),
                        username: Some("synthetic-user".into()),
                        plan: Some(AccountPlan::Premium),
                        expires_at: None,
                    }),
                    refreshed,
                },
            }
        })
    }

    fn unrestrict<'a>(
        &'a self,
        credential: &'a Secret,
        link: &'a Url,
    ) -> BoxFuture<'a, WithCredentials<Unrestricted>> {
        Box::pin(async move {
            self.unrestricts.fetch_add(1, Ordering::SeqCst);
            let refreshed = match self.present(credential).await {
                Err(()) => {
                    return WithCredentials::unchanged(Unrestricted::refused(
                        FailureCode::InvalidCredentials,
                        "stale credentials",
                    ));
                }
                Ok(refreshed) => refreshed,
            };
            let answer = self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Answer::Generate);
            let result = match answer {
                Answer::Generate => Unrestricted::Generated(self.generate(link)),
                Answer::GenerateThenLose => {
                    self.generate(link);
                    Unrestricted::Ambiguous("connection reset".into())
                }
                Answer::LoseRequest => Unrestricted::Ambiguous("connection reset".into()),
                Answer::Refuse(code, message, retry_after) => Unrestricted::Refused {
                    code,
                    message: message.into(),
                    retry_after,
                },
                Answer::GenerateThenWait => {
                    let link = self.generate(link);
                    self.gate.acquire().await.unwrap().forget();
                    Unrestricted::Generated(link)
                }
                Answer::WaitThenGenerate => {
                    self.gate.acquire().await.unwrap().forget();
                    Unrestricted::Generated(self.generate(link))
                }
            };
            WithCredentials { result, refreshed }
        })
    }

    fn find_generated<'a>(
        &'a self,
        credential: &'a Secret,
        link: &'a Url,
        _since: UtcTimestamp,
    ) -> BoxFuture<'a, WithCredentials<Reconciled>> {
        Box::pin(async move {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let refreshed = match self.present(credential).await {
                Err(()) => {
                    return WithCredentials::unchanged(Reconciled::Unknown("stale".into()));
                }
                Ok(refreshed) => refreshed,
            };
            let found = self
                .record
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(original, _)| original == link.as_str())
                .map(|(_, generated)| generated.clone());
            WithCredentials {
                result: found.map_or(Reconciled::NotFound, Reconciled::Found),
                refreshed,
            }
        })
    }
}

/// One file the download host serves.
#[derive(Debug, Clone)]
struct File {
    body: Bytes,
    etag: String,
    stall_at: Option<usize>,
    /// Refuse requests as for an expired link.
    refuse: bool,
}

/// The download host: one file served at every generated link, and every
/// request it saw.
#[derive(Default)]
struct Host {
    default: Mutex<Option<File>>,
    requests: Mutex<Vec<OpenRequest>>,
    refused_paths: Mutex<HashSet<String>>,
}

impl Host {
    fn serve(&self, data: Bytes) {
        *self.default.lock().unwrap() = Some(File {
            body: data,
            etag: "\"v1\"".into(),
            stall_at: None,
            refuse: false,
        });
    }

    fn change(&self, change: impl FnOnce(&mut File)) {
        change(self.default.lock().unwrap().as_mut().unwrap());
    }

    fn file(&self, path: &str) -> Option<File> {
        let mut file = self.default.lock().unwrap().clone()?;
        file.refuse |= self.refused_paths.lock().unwrap().contains(path);
        Some(file)
    }

    fn requests(&self) -> Vec<(String, Option<Range>)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| (r.url.as_str().to_owned(), r.range))
            .collect()
    }
}

struct Body {
    host: Arc<Host>,
    path: String,
    data: Bytes,
    offset: usize,
    position: usize,
    chunk: usize,
}

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
        Box::pin(async move {
            loop {
                let stall = self.host.file(&self.path).and_then(|f| f.stall_at);
                let at = self.offset + self.position;
                match stall {
                    Some(limit) if at >= limit => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    _ => {
                        if self.position == self.data.len() {
                            return Ok(None);
                        }
                        let mut end = (self.position + self.chunk).min(self.data.len());
                        if let Some(limit) = stall {
                            end = end.min(self.position + (limit - at));
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

struct HostTransport(Arc<Host>);

impl Transport for HostTransport {
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        Box::pin(async move {
            let host = &self.0;
            host.requests.lock().unwrap().push(request.clone());
            let path = request.url.path().to_owned();
            let file = match request.url.host_str() {
                Some(CDN) => host.file(&path),
                // Only a direct job asks another host.
                _ => host.default.lock().unwrap().clone(),
            };
            let Some(file) = file else {
                return Err(TransportError {
                    code: FailureCode::LeaseNotFound,
                    message: "HTTP 404".into(),
                });
            };
            if file.refuse {
                return Err(TransportError {
                    code: FailureCode::LeaseExpired,
                    message: "HTTP 403".into(),
                });
            }
            let len = file.body.len() as u64;
            let representation = Representation {
                length: Some(len),
                strong_etag: Some(file.etag.clone()),
                last_modified: None,
                // As the real transport: a digest of the URL that served
                // it, never the URL, which may carry a signature.
                resource: Some({
                    use std::hash::{Hash, Hasher};
                    let mut digest = std::collections::hash_map::DefaultHasher::new();
                    request.url.as_str().hash(&mut digest);
                    format!("{:016x}", digest.finish())
                }),
            };
            let matches = request
                .validators
                .as_ref()
                .is_none_or(|v| v.strong_etag == representation.strong_etag);
            let (framing, start) = match request.range {
                Some((start, None)) if matches => (
                    Framing::Partial {
                        first: start,
                        last: len - 1,
                        complete: Some(len),
                    },
                    start,
                ),
                _ => (Framing::Full { length: Some(len) }, 0),
            };
            let start = usize::try_from(start).unwrap();
            Ok(Response {
                framing,
                representation,
                filename: None,
                body: Box::new(Body {
                    host: host.clone(),
                    path,
                    data: file.body.slice(start..),
                    offset: start,
                    position: 0,
                    chunk: request.read_buffer,
                }),
            })
        })
    }
}

struct Servers {
    provider: Arc<Provider>,
    host: Arc<Host>,
}

type Env = support::Env<Servers>;

fn env() -> Env {
    let provider = Provider::new();
    let host = Arc::<Host>::default();
    let servers = Servers {
        provider: provider.clone(),
        host: host.clone(),
    };
    let mut env = Env::new(servers, Arc::new(HostTransport(host)));
    env.providers.push(provider);
    env.credential_key = Some(env.state.parent().unwrap().join("credential.key"));
    env
}

/// Signs an account in and returns it.
async fn add_account(owner: &EngineOwner, provider: &Provider) -> AccountId {
    authorize(owner, provider, None).await
}

/// Signs `account` in again, replacing its credentials.
async fn sign_in_again(owner: &EngineOwner, provider: &Provider, account: AccountId) {
    let revision = account_revision(owner, account).await;
    let again = AccountRef { account, revision };
    assert_eq!(authorize(owner, provider, Some(again)).await, account);
}

async fn authorize(
    owner: &EngineOwner,
    provider: &Provider,
    account: Option<AccountRef>,
) -> AccountId {
    let alias = account
        .is_none()
        .then(|| AccountAlias::new("Main").unwrap());
    let command = Command::AuthorizeAccount {
        service: ProviderId::new(SERVICE).unwrap(),
        account,
        alias,
    };
    let started = match owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap()
    {
        CommandResult::AuthorizationStarted(a) => a,
        other => panic!("{other:?}"),
    };
    provider.approvals.add_permits(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let query = Query::Authorization { id: started.id };
        if let QueryResult::Authorization(a) = owner.handle().query(query, ALL).await.unwrap()
            && a.state != AuthorizationState::Pending
        {
            assert_eq!(a.state, AuthorizationState::Completed, "{a:?}");
            return a.account.unwrap();
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn account_revision(
    owner: &EngineOwner,
    id: AccountId,
) -> wayfetch_protocol::domain::version::Revision {
    match owner.handle().query(Query::Accounts {}, ALL).await.unwrap() {
        QueryResult::Accounts(a) => a.accounts.iter().find(|a| a.id == id).unwrap().revision,
        other => panic!("{other:?}"),
    }
}

async fn collect(owner: &EngineOwner, links: &[&str]) -> Vec<CollectedLink> {
    let items = links.iter().map(|l| (*l).to_owned()).collect::<Vec<_>>();
    let command = Command::CollectLinks {
        items: CollectItems::try_from(items).unwrap(),
        destination: None,
    };
    owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap();
    listed(owner).await
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

fn start_command(links: &[CollectedLink], account: Option<AccountId>) -> Command {
    let refs = links
        .iter()
        .map(|l| LinkRef {
            link: l.id,
            revision: l.revision,
        })
        .collect::<Vec<_>>();
    Command::StartLinks {
        links: LinkRefs::try_from(refs).unwrap(),
        account,
    }
}

/// Starts `links` through `account` and returns their jobs.
async fn start(
    owner: &EngineOwner,
    links: &[CollectedLink],
    account: Option<AccountId>,
) -> Result<Vec<JobId>, ProtocolError> {
    let command = start_command(links, account);
    match owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await?
    {
        CommandResult::LinksApplied { applied, refused } => {
            assert!(refused.is_empty(), "{refused:?}");
            Ok(applied.iter().map(|a| a.job.unwrap()).collect())
        }
        other => panic!("{other:?}"),
    }
}

async fn start_one(owner: &EngineOwner, account: Option<AccountId>) -> JobId {
    let links = collect(owner, &[ORIGINAL]).await;
    start(owner, &links[links.len() - 1..], account)
        .await
        .unwrap()[0]
}

async fn failed(owner: &EngineOwner, job: JobId) -> JobSummary {
    wait_for(owner, job, |j| {
        assert_ne!(j.stage, JobStage::Complete, "{j:?}");
        j.stage == JobStage::Failed
    })
    .await
}

async fn retry(
    owner: &EngineOwner,
    job: &JobSummary,
    repeat: bool,
) -> (CommandEnvelope, Result<CommandResult, ProtocolError>) {
    let envelope = envelope(
        owner,
        Command::Retry {
            job: job.id,
            expected_revision: job.revision,
            repeat_provider_operation: repeat,
        },
    );
    let result = owner.handle().execute(envelope.clone(), ALL).await;
    (envelope, result)
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Every file in the state directory, the database's WAL included.
fn state_files(state: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut dirs = vec![state.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                out.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    out
}

fn assert_no_secret_in(text: &[u8], what: &str) {
    for secret in [SIGNATURE, CREDENTIAL, CDN] {
        assert!(!contains(text, secret), "{secret} in {what}");
    }
}

async fn job(owner: &EngineOwner, id: JobId) -> JobSummary {
    support::job(owner, id).await
}

/// A second connection to the engine's database, as a fault injector.
fn database(env: &Env) -> rusqlite::Connection {
    let db = rusqlite::Connection::open(env.state.join("state.sqlite3")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db
}

/// The states of every provider operation, oldest first.
fn operation_states(env: &Env) -> Vec<String> {
    let db = database(env);
    let mut query = db
        .prepare("SELECT state FROM provider_operations ORDER BY id")
        .unwrap();
    query
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The generated links the download host was asked for, by number.
fn links_asked(host: &Host) -> Vec<String> {
    let mut asked = host
        .requests()
        .into_iter()
        .filter_map(|(url, _)| url.split("/d/").nth(1).map(|n| n[..1].to_owned()))
        .collect::<Vec<_>>();
    asked.dedup();
    asked
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_started_through_an_account_downloads_its_exact_bytes() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(40 * KIB + 3);
    host.serve(data.clone());
    *provider.size.lock().unwrap() = Some(data.len() as u64);
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;

    // Collecting, listing, and reading accounts never generate a link.
    let links = collect(&owner, &[ORIGINAL]).await;
    owner.handle().query(Query::Accounts {}, ALL).await.unwrap();
    listed(&owner).await;
    assert_eq!(provider.unrestricts(), 0);
    assert!(host.requests().is_empty());

    let [id] = start(&owner, &links, Some(account)).await.unwrap()[..] else {
        panic!()
    };
    let done = support::complete(&owner, id).await;
    assert_eq!(
        done.route,
        Some(Box::new(JobRoute {
            service: ProviderId::new(SERVICE).unwrap(),
            account
        }))
    );
    // The provider's name for the file, since the host gave none.
    assert_eq!(done.artifact.as_ref().unwrap().path, "Provider Name.bin");
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(provider.unrestricts(), 1);
    // Only the generated link was requested, never the original host.
    let requests = host.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].0.starts_with(&format!("https://{CDN}/d/1/")));

    // No generated link or credential in what clients see, nor in the
    // database or its WAL.
    let seen = serde_json::to_vec(&(
        support::jobs(&owner).await,
        listed(&owner).await,
        owner.handle().query(Query::Accounts {}, ALL).await.unwrap(),
    ))
    .unwrap();
    assert_no_secret_in(&seen, "client views");
    let subscription = owner
        .handle()
        .subscribe(wayfetch_protocol::Subscribe { after: None }, ALL)
        .await
        .unwrap();
    assert_no_secret_in(format!("{subscription:?}").as_bytes(), "events");
    for (path, bytes) in state_files(&env.state) {
        assert_no_secret_in(&bytes, &path.display().to_string());
    }
    shutdown(owner).await;
    for (path, bytes) in state_files(&env.state) {
        assert_no_secret_in(&bytes, &path.display().to_string());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_downloads_never_ask_the_provider() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(5 * KIB));
    let owner = env.start().await;
    add_account(&owner, provider).await;
    let submitted = support::submit(&owner, ["https://files.example/a.bin".to_owned()]).await;
    let collected = start_one(&owner, None).await;
    for id in [submitted[0], collected] {
        let done = support::complete(&owner, id).await;
        assert_eq!(done.route, None);
    }
    assert_eq!(provider.unrestricts(), 0);
    assert_eq!(provider.lookups(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_and_account_downloads_share_the_total_speed_limit() {
    use wayfetch_engine::transfer::throttle::{burst, quantum};
    use wayfetch_protocol::query::SpeedLimit;

    let mut env = env();
    let limit = 64 * KIB as u64;
    env.policy.download_speed_limit.default = Some(SpeedLimit::BytesPerSecond(limit));
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(96 * KIB);
    host.serve(data.clone());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let began = tokio::time::Instant::now();
    let direct = support::submit(&owner, ["https://files.example/direct.bin".to_owned()]).await[0];
    let routed = start_one(&owner, Some(account)).await;
    assert!(support::complete(&owner, direct).await.route.is_none());
    assert_eq!(
        support::complete(&owner, routed)
            .await
            .route
            .unwrap()
            .account,
        account
    );
    // Both transfers must spend the same budget, including any credit
    // accumulated while signing in. Separate budgets would finish sooner.
    let floor = (2 * data.len() as u64 - burst(limit) - quantum(limit)) as f64 / limit as f64;
    assert!(began.elapsed().as_secs_f64() >= floor);
    assert_eq!(env.published("direct.bin"), data);
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(provider.unrestricts(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn using_an_account_needs_the_provider_downloads_permission() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let links = collect(&owner, &[ORIGINAL]).await;
    // Neither submitting nor managing accounts grants using one.
    let managing = [
        Permission::Read,
        Permission::Submit,
        Permission::Control,
        Permission::Sources,
        Permission::Accounts,
    ];
    for granted in [&managing[..3], &managing[..]] {
        let refused = owner
            .handle()
            .execute(
                envelope(&owner, start_command(&links, Some(account))),
                granted,
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::PermissionDenied, "{granted:?}");
    }
    assert!(listed(&owner).await[0].job.is_none(), "the link stays");
    assert_eq!(provider.unrestricts(), 0);
    // Using an account needs no right to see or manage accounts.
    let using = [Permission::Submit, Permission::ProviderDownloads];
    assert_eq!(
        owner
            .handle()
            .query(Query::Accounts {}, &using)
            .await
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    owner
        .handle()
        .execute(
            envelope(&owner, start_command(&links, Some(account))),
            &using,
        )
        .await
        .unwrap();
    assert!(listed(&owner).await.is_empty());
    let id = support::jobs(&owner).await[0].id;
    support::complete(&owner, id).await;
    assert_eq!(provider.unrestricts(), 1);
    // A client without it still starts links directly.
    let more = collect(&owner, &["https://hoster.example/file/direct"]).await;
    owner
        .handle()
        .execute(envelope(&owner, start_command(&more, None)), &managing[..3])
        .await
        .unwrap();
    assert_eq!(provider.unrestricts(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_through_an_unusable_account_keeps_the_links() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let links = collect(&owner, &[ORIGINAL]).await;

    let unknown = AccountId::from_u64(99).unwrap();
    let err = start(&owner, &links, Some(unknown)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    let revision = account_revision(&owner, account).await;
    let disable = Command::SetAccountEnabled {
        account,
        expected_revision: revision,
        enabled: false,
    };
    owner
        .handle()
        .execute(envelope(&owner, disable), ALL)
        .await
        .unwrap();
    let err = start(&owner, &links, Some(account)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    assert!(err.message.as_str().contains("disabled"), "{err:?}");

    let revision = account_revision(&owner, account).await;
    let remove = Command::RemoveAccount {
        account,
        expected_revision: revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, remove), ALL)
        .await
        .unwrap();
    let err = start(&owner, &links, Some(account)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);

    // Every refusal left the link collected and not started.
    let now = listed(&owner).await;
    assert_eq!(now.len(), 1);
    assert!(now[0].job.is_none());
    assert_eq!(provider.unrestricts(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_vault_refuses_account_downloads_but_not_direct_ones() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    shutdown(owner).await;
    // Another key: the stored credentials cannot be opened.
    std::fs::remove_file(env.credential_key.as_ref().unwrap()).unwrap();
    let owner = env.start().await;
    let links = collect(&owner, &[ORIGINAL]).await;
    let err = start(&owner, &links, Some(account)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    assert!(err.message.as_str().contains("credential"), "{err:?}");
    let id = start(&owner, &links, None).await.unwrap()[0];
    support::complete(&owner, id).await;
    assert_eq!(provider.unrestricts(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn checks_and_link_requests_keep_the_newest_credentials() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(8 * KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    // Every call rotates the credential and takes a while, so calls that
    // overlapped would present one already replaced.
    *provider.rotate.lock().unwrap() = true;
    *provider.delay.lock().unwrap() = Duration::from_millis(30);
    let links = collect(
        &owner,
        &[
            "https://hoster.example/file/1",
            "https://hoster.example/file/2",
            "https://hoster.example/file/3",
        ],
    )
    .await;
    let handle = owner.handle();
    let checks = tokio::spawn(async move {
        for _ in 0..3 {
            handle.refresh_account(account, ALL).await.unwrap();
        }
    });
    let jobs = start(&owner, &links, Some(account)).await.unwrap();
    for id in &jobs {
        support::complete(&owner, *id).await;
    }
    checks.await.unwrap();
    assert_eq!(provider.stale.load(Ordering::SeqCst), 0);
    assert_eq!(provider.most_running.load(Ordering::SeqCst), 1);
    assert_eq!(provider.unrestricts(), 3);
    // The stored credential is the newest: one more check succeeds.
    let checked = owner.handle().refresh_account(account, ALL).await.unwrap();
    assert_eq!(
        checked.status,
        wayfetch_protocol::account::AccountStatus::Authenticated
    );
    assert_eq!(provider.stale.load(Ordering::SeqCst), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_refusals_fail_the_download_saying_what_to_do() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;

    provider.answer(Answer::Refuse(
        FailureCode::HostUnavailableViaProvider,
        "The provider does not support this host.",
        None,
    ));
    let id = start_one(&owner, Some(account)).await;
    let job = failed(&owner, id).await;
    let failure = job.failure.unwrap();
    assert_eq!(failure.code, FailureCode::HostUnavailableViaProvider);
    assert_eq!(
        failure.message.unwrap().as_str(),
        "The provider does not support this host."
    );

    provider.answer(Answer::Refuse(
        FailureCode::QuotaExhausted,
        "Slow down.",
        Some(Duration::from_secs(60)),
    ));
    let id = start_one(&owner, Some(account)).await;
    let job = failed(&owner, id).await;
    assert_eq!(
        job.failure.as_ref().unwrap().code,
        FailureCode::QuotaExhausted
    );
    assert!(job.failure.as_ref().unwrap().retry_at.is_some());
    // Not before the provider's deadline.
    let (_, early) = retry(&owner, &job, false).await;
    assert_eq!(early.unwrap_err().code, ErrorCode::NotApplicable);

    // One request each, and nothing was downloaded.
    assert_eq!(provider.unrestricts(), 2);
    assert!(host.requests().is_empty());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_answer_is_reconciled_and_not_repeated() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(6 * KIB);
    host.serve(data.clone());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::GenerateThenLose);
    let id = start_one(&owner, Some(account)).await;
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!((provider.unrestricts(), provider.lookups()), (1, 1));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_outcome_waits_for_the_user_to_confirm_a_repeat() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(3 * KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::LoseRequest);
    let id = start_one(&owner, Some(account)).await;
    let job = failed(&owner, id).await;
    assert_eq!(
        job.failure.as_ref().unwrap().code,
        FailureCode::OperationAmbiguous
    );
    assert_eq!((provider.unrestricts(), provider.lookups()), (1, 1));

    // A plain retry is refused, saying why.
    let (_, plain) = retry(&owner, &job, false).await;
    let err = plain.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    assert!(err.message.as_str().contains("repeat"), "{err:?}");
    // Confirming needs the provider downloads permission.
    let confirm = Command::Retry {
        job: id,
        expected_revision: job.revision,
        repeat_provider_operation: true,
    };
    let err = owner
        .handle()
        .execute(
            envelope(&owner, confirm),
            &[Permission::Read, Permission::Submit, Permission::Control],
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PermissionDenied);

    // Confirmed: the record is read once more, then the provider asked.
    let (confirmed, result) = retry(&owner, &job, true).await;
    result.unwrap();
    support::complete(&owner, id).await;
    assert_eq!((provider.unrestricts(), provider.lookups()), (2, 2));
    // The same command again only replays its result.
    owner.handle().execute(confirmed, ALL).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!((provider.unrestricts(), provider.lookups()), (2, 2));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_whose_outcome_was_not_saved_is_never_used() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(3 * KIB);
    host.serve(data.clone());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    // Recording any operation's outcome fails.
    database(&env)
        .execute_batch(
            "CREATE TRIGGER fail_outcomes BEFORE UPDATE OF state ON provider_operations \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    let id = start_one(&owner, Some(account)).await;
    let job = failed(&owner, id).await;
    let failure = job.failure.clone().unwrap();
    assert_eq!(failure.code, FailureCode::OperationAmbiguous);
    assert!(
        failure
            .message
            .unwrap()
            .as_str()
            .contains("could not record"),
        "{job:?}"
    );
    assert!(host.requests().is_empty(), "the link was released");
    assert_eq!(operation_states(&env), ["intended"]);
    assert_eq!(provider.unrestricts(), 1);

    // Once storage works again, a confirmed retry finds the link the
    // provider made, without asking again.
    database(&env)
        .execute_batch("DROP TRIGGER fail_outcomes;")
        .unwrap();
    let (_, plain) = retry(&owner, &job, false).await;
    assert_eq!(plain.unwrap_err().code, ErrorCode::NotApplicable);
    retry(&owner, &job, true).await.1.unwrap();
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!((provider.unrestricts(), provider.lookups()), (1, 1));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_that_was_not_saved_can_be_retried() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(3 * KIB);
    host.serve(data.clone());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    database(&env)
        .execute_batch(
            "CREATE TRIGGER fail_outcomes BEFORE UPDATE OF state ON provider_operations \
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    provider.answer(Answer::Refuse(
        FailureCode::HostUnavailableViaProvider,
        "Real-Debrid cannot reach this host right now.",
        None,
    ));
    let id = start_one(&owner, Some(account)).await;
    // The refusal is not recorded, so the operation stays open: the job
    // says its outcome is unresolved, which the confirmation answers.
    let job = failed(&owner, id).await;
    let failure = job.failure.clone().unwrap();
    assert_eq!(failure.code, FailureCode::OperationAmbiguous, "{job:?}");
    assert!(
        failure
            .message
            .unwrap()
            .as_str()
            .contains("could not record")
    );
    assert_eq!(operation_states(&env), ["intended"]);
    database(&env)
        .execute_batch("DROP TRIGGER fail_outcomes;")
        .unwrap();
    let (_, plain) = retry(&owner, &job, false).await;
    assert_eq!(plain.unwrap_err().code, ErrorCode::NotApplicable);
    retry(&owner, &job, true).await.1.unwrap();
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!((provider.unrestricts(), provider.lookups()), (2, 1));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_for_replaced_credentials_is_never_used() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(3 * KIB);
    host.serve(data.clone());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::WaitThenGenerate);
    let id = start_one(&owner, Some(account)).await;
    while provider.unrestricts() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Signed in again while the provider is still answering the request
    // made with the old credentials.
    sign_in_again(&owner, provider, account).await;
    provider.gate.add_permits(1);
    let job = failed(&owner, id).await;
    let failure = job.failure.clone().unwrap();
    assert_eq!(failure.code, FailureCode::InvalidCredentials);
    assert!(
        failure
            .message
            .unwrap()
            .as_str()
            .contains("signed in again"),
        "{job:?}"
    );
    assert!(host.requests().is_empty(), "the old link was used");
    assert_eq!(operation_states(&env), ["retired"]);

    // A retry asks with the current sign-in.
    retry(&owner, &job, false).await.1.unwrap();
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(provider.unrestricts(), 2);
    assert_eq!(links_asked(host), ["2"]);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_link_from_an_earlier_sign_in_is_replaced() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(64 * KIB);
    host.serve(data.clone());
    host.change(|f| f.stall_at = Some(16 * KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let id = start_one(&owner, Some(account)).await;
    let running = wait_for(&owner, id, |j| j.bytes_done >= 8 * KIB as u64).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: running.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let paused = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    // The paused download's stored link came with the old sign-in.
    sign_in_again(&owner, provider, account).await;
    host.change(|f| f.stall_at = None);
    let resume = Command::Resume {
        job: id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(provider.unrestricts(), 2);
    assert_eq!(links_asked(host), ["1", "2"]);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_another_request_may_have_made_is_not_taken() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(3 * KIB);
    host.serve(data.clone());
    // The first link is refused, so a second is asked for, and its answer
    // is lost: the provider's record then lists two entries for the same
    // link, and the newest is not proven to be the second request's.
    host.refused_paths
        .lock()
        .unwrap()
        .insert("/d/1/file.bin".into());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::Generate);
    provider.answer(Answer::GenerateThenLose);
    let id = start_one(&owner, Some(account)).await;
    let job = failed(&owner, id).await;
    assert_eq!(
        job.failure.as_ref().unwrap().code,
        FailureCode::OperationAmbiguous
    );
    assert_eq!((provider.unrestricts(), provider.lookups()), (2, 1));
    assert_eq!(links_asked(host), ["1"]);

    // Confirmed: still not provable, so the provider is asked again.
    retry(&owner, &job, true).await.1.unwrap();
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!((provider.unrestricts(), provider.lookups()), (3, 2));
    assert_eq!(links_asked(host), ["1", "3"]);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_after_the_provider_answered_is_reconciled_on_restart() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(4 * KIB);
    host.serve(data.clone());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::GenerateThenWait);
    let id = start_one(&owner, Some(account)).await;
    while provider.record.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The engine stops before the answer arrives.
    shutdown(owner).await;
    provider.gate.add_permits(1);

    let owner = env.start().await;
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(provider.unrestricts(), 1, "never asked again");
    assert_eq!(provider.lookups(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crash_with_no_answer_is_ambiguous_after_restart() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::WaitThenGenerate);
    let id = start_one(&owner, Some(account)).await;
    while provider.unrestricts() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The intent was recorded and the request may have been sent.
    shutdown(owner).await;
    let owner = env.start().await;
    let job = failed(&owner, id).await;
    assert_eq!(
        job.failure.as_ref().unwrap().code,
        FailureCode::OperationAmbiguous
    );
    assert_eq!((provider.unrestricts(), provider.lookups()), (1, 1));
    provider.gate.add_permits(1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_restart_continue_on_the_same_link() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(64 * KIB);
    host.serve(data.clone());
    host.change(|f| f.stall_at = Some(20 * KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let id = start_one(&owner, Some(account)).await;
    let paused = wait_for(&owner, id, |j| j.bytes_done >= 16 * KIB as u64).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let paused = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    host.change(|f| f.stall_at = Some(40 * KIB));
    let resume = Command::Resume {
        job: id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    wait_for(&owner, id, |j| j.bytes_done >= 36 * KIB as u64).await;
    // The engine stops mid-transfer and continues after restarting.
    shutdown(owner).await;
    host.change(|f| f.stall_at = None);
    let owner = env.start().await;
    let done = support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(done.restart_reason, None);
    assert_eq!(provider.unrestricts(), 1);
    let requests = host.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert!(requests.iter().all(|(url, _)| *url == requests[0].0));
    assert_eq!(requests[0].1, None);
    assert!(requests[1..].iter().all(|(_, range)| range.is_some()));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_file_restarts_from_zero_on_the_same_link() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(32 * KIB));
    host.change(|f| f.stall_at = Some(12 * KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let id = start_one(&owner, Some(account)).await;
    let running = wait_for(&owner, id, |j| j.bytes_done >= 8 * KIB as u64).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: running.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let paused = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;
    let changed = body(32 * KIB).iter().map(|b| b ^ 0xff).collect::<Bytes>();
    host.change(|f| {
        f.body = changed.clone();
        f.etag = "\"v2\"".into();
        f.stall_at = None;
    });
    let resume = Command::Resume {
        job: id,
        expected_revision: paused.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    let done = support::complete(&owner, id).await;
    assert!(done.restart_reason.is_some(), "{done:?}");
    assert_eq!(env.published("Provider Name.bin"), changed);
    assert_eq!(provider.unrestricts(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_the_host_refuses_is_replaced_once() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(5 * KIB);
    host.serve(data.clone());
    host.refused_paths
        .lock()
        .unwrap()
        .insert("/d/1/file.bin".into());
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let id = start_one(&owner, Some(account)).await;
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);
    assert_eq!(provider.unrestricts(), 2);

    // Refused again: the download fails rather than asking forever.
    host.refused_paths.lock().unwrap().extend([
        "/d/2/file.bin".to_owned(),
        "/d/3/file.bin".into(),
        "/d/4/file.bin".into(),
    ]);
    let other = collect(&owner, &["https://hoster.example/file/other"]).await;
    let id = start(&owner, &other[other.len() - 1..], Some(account))
        .await
        .unwrap()[0];
    let job = failed(&owner, id).await;
    assert_eq!(job.failure.unwrap().code, FailureCode::LeaseExpired);
    assert_eq!(provider.unrestricts(), 4);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_while_resolving_never_publishes_the_late_answer() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    provider.answer(Answer::WaitThenGenerate);
    let id = start_one(&owner, Some(account)).await;
    while provider.unrestricts() == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let current = job(&owner, id).await;
    let cancel = Command::Cancel {
        job: id,
        expected_revision: current.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, cancel), ALL)
        .await
        .unwrap();
    wait_for(&owner, id, |j| j.stage == JobStage::Cancelled).await;
    // The provider answers after the cancel.
    provider.gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = job(&owner, id).await;
    assert_eq!(after.stage, JobStage::Cancelled);
    assert!(after.artifact.is_none());
    assert!(host.requests().is_empty());
    shutdown(owner).await;
    // The late link was never stored.
    for (path, bytes) in state_files(&env.state) {
        assert_no_secret_in(&bytes, &path.display().to_string());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disabling_or_removing_the_account_stops_its_downloads() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(32 * KIB));
    host.change(|f| f.stall_at = Some(8 * KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let id = start_one(&owner, Some(account)).await;
    wait_for(&owner, id, |j| j.stage == JobStage::Transferring).await;

    let revision = account_revision(&owner, account).await;
    let disable = Command::SetAccountEnabled {
        account,
        expected_revision: revision,
        enabled: false,
    };
    owner
        .handle()
        .execute(envelope(&owner, disable), ALL)
        .await
        .unwrap();
    let job = failed(&owner, id).await;
    assert!(
        job.failure
            .unwrap()
            .message
            .unwrap()
            .as_str()
            .contains("disabled"),
    );
    host.change(|f| f.stall_at = None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(support::job(&owner, id).await.artifact.is_none());

    // Removed while a retried download waits to run: it fails saying so.
    let revision = account_revision(&owner, account).await;
    let remove = Command::RemoveAccount {
        account,
        expected_revision: revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, remove), ALL)
        .await
        .unwrap();
    let job = support::job(&owner, id).await;
    retry(&owner, &job, false).await.1.unwrap();
    let job = failed(&owner, id).await;
    assert!(
        job.failure
            .unwrap()
            .message
            .unwrap()
            .as_str()
            .contains("removed")
    );
    assert_eq!(job.route.unwrap().account, account, "the route stays shown");
    assert_eq!(provider.unrestricts(), 1);
    shutdown(owner).await;
}
