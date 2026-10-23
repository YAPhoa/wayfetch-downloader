//! Downloads through a provider account, through the public engine API, with
//! an in-process provider and download host (ADR-09, ADR-24). Real-Debrid's
//! own client is tested in `wayfetch-adapters`.

mod support;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, KIB, body, envelope, shutdown, wait_for};
use tokio::sync::Semaphore;
use url::Url;
use wayfetch_engine::EngineOwner;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, Availability, Checked, DeviceAuthorization, GeneratedLink, PollOutcome,
    Profile, ProviderError, Reconciled, SupportedHosts, Unrestricted, WithCredentials,
};
use wayfetch_engine::crawl::{InspectError, InspectRequest, Inspection, Inspector};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_protocol::account::{
    AccountAlias, AccountPlan, AccountRef, AuthorizationMethod, AuthorizationState, ServiceSupport,
};
use wayfetch_protocol::command::{CollectMode, CollectText, LinkRef, LinkRefs};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::{AccountId, JobId};
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::name::ProviderId;
use wayfetch_protocol::domain::time::UtcTimestamp;
use wayfetch_protocol::query::{
    CheckReason, CollectedLink, JobRoute, JobSummary, LinkCheck, PageSize,
};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, Permission, ProtocolError, Query,
    QueryResult,
};

const SERVICE: &str = "synthetic-debrid";
/// A hoster link, as collected.
const ORIGINAL: &str = "https://hoster.example/file/abc123";
/// A fixture provider's own account-only link, never a live provider URL.
const NATIVE: &str = "https://provider.example/d/SYNTHETIC123";
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
    hosts: AtomicUsize,
    domains: Mutex<Vec<String>>,
    host_error: Mutex<Option<ProviderError>>,
    hosts_wait: AtomicBool,
    hosts_panic: AtomicBool,
    hosts_gate: Semaphore,
    user: Mutex<String>,
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
    /// What a link check is told, instead of the file it would generate.
    availability: Mutex<Option<Availability>>,
    availabilities: AtomicUsize,
    availability_panic: AtomicBool,
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
            hosts: AtomicUsize::new(0),
            domains: Mutex::new(vec!["hoster.example".into()]),
            host_error: Mutex::new(None),
            hosts_wait: AtomicBool::new(false),
            hosts_panic: AtomicBool::new(false),
            hosts_gate: Semaphore::new(0),
            user: Mutex::new("user-1".into()),
            running: AtomicUsize::new(0),
            most_running: AtomicUsize::new(0),
            answers: Mutex::new(VecDeque::new()),
            record: Mutex::new(Vec::new()),
            gate: Semaphore::new(0),
            approvals: Semaphore::new(0),
            delay: Mutex::new(Duration::ZERO),
            filename: Mutex::new(Some("Provider Name.bin".into())),
            size: Mutex::new(None),
            availability: Mutex::new(None),
            availabilities: AtomicUsize::new(0),
            availability_panic: AtomicBool::new(false),
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

    fn requires_account(&self, url: &Url) -> bool {
        self.service.as_str() == SERVICE && url.as_str() == NATIVE
    }

    fn supported_hosts(&self) -> BoxFuture<'_, Result<SupportedHosts, ProviderError>> {
        Box::pin(async {
            self.hosts.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.hosts_panic.load(Ordering::SeqCst),
                "synthetic supported-host panic"
            );
            if self.hosts_wait.load(Ordering::SeqCst) {
                self.hosts_gate.acquire().await.unwrap().forget();
            }
            if let Some(error) = self.host_error.lock().unwrap().clone() {
                return Err(error);
            }
            SupportedHosts::new(self.domains.lock().unwrap().clone())
        })
    }

    fn availability<'a>(&'a self, _link: &'a Url) -> BoxFuture<'a, Availability> {
        Box::pin(async {
            self.availabilities.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.availability_panic.load(Ordering::SeqCst),
                "synthetic availability panic"
            );
            self.availability
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Availability::File {
                    name: self.filename.lock().unwrap().clone(),
                    size: *self.size.lock().unwrap(),
                })
        })
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
                    user_id: Some(self.user.lock().unwrap().clone()),
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
                        user_id: Some(self.user.lock().unwrap().clone()),
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
    filename: Option<String>,
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
            filename: None,
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
                    status: Some(404),
                });
            };
            if file.refuse {
                return Err(TransportError {
                    code: FailureCode::LeaseExpired,
                    message: "HTTP 403".into(),
                    status: Some(403),
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
                filename: file.filename,
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
        service: provider.service.clone(),
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
    let command = Command::CollectLinks {
        text: CollectText::try_from(links.join("\n")).unwrap(),
        base_url: None,
        mode: CollectMode::Analyze,
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
        routing: account
            .is_none()
            .then_some(wayfetch_protocol::command::StartRouting::Direct),
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
    assert_eq!(done.name.as_deref(), Some("Provider Name.bin"));
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
    host.change(|f| {
        f.stall_at = Some(20 * KIB);
        f.filename = Some("Response Name.bin".into());
    });
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let id = start_one(&owner, Some(account)).await;
    let paused = wait_for(&owner, id, |j| j.bytes_done >= 16 * KIB as u64).await;
    assert_eq!(paused.name.as_deref(), Some("Response Name.bin"));
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
    let resumed = wait_for(&owner, id, |j| j.bytes_done >= 36 * KIB as u64).await;
    assert_eq!(resumed.name.as_deref(), Some("Response Name.bin"));
    // The engine stops mid-transfer and continues after restarting.
    shutdown(owner).await;
    host.change(|f| f.stall_at = None);
    let owner = env.start().await;
    let done = support::complete(&owner, id).await;
    assert_eq!(env.published("Response Name.bin"), data);
    assert_eq!(done.name.as_deref(), Some("Response Name.bin"));
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
    // The accepted provider name is visible even when opening its bytes
    // never returns a successful response or creates a staging file.
    assert_eq!(job.name.as_deref(), Some("Provider Name.bin"));
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
    assert_eq!(after.name, None);
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

fn automatic_command(links: &[CollectedLink]) -> Command {
    let mut command = start_command(links, None);
    if let Command::StartLinks { routing, .. } = &mut command {
        *routing = None;
    }
    command
}

fn applied(
    result: CommandResult,
) -> (
    Vec<wayfetch_protocol::command::AppliedLink>,
    Vec<wayfetch_protocol::command::RefusedLink>,
) {
    match result {
        CommandResult::LinksApplied { applied, refused } => (applied, refused),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_start_routes_each_hostname_and_replays_without_provider_work() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    let urls = [
        ORIGINAL,
        "https://CDN.HOSTER.EXAMPLE/file/subdomain",
        "https://nothoster.example/file/false-prefix",
        "https://hoster.example.evil.test/file/false-suffix",
        "https://other.example/?next=https://hoster.example/",
    ];
    let links = collect(&owner, &urls).await;
    let command = envelope(&owner, automatic_command(&links));
    let first = owner
        .handle()
        .execute(
            command.clone(),
            &[Permission::Submit, Permission::ProviderDownloads],
        )
        .await
        .unwrap();
    let (done, refused) = applied(first.clone());
    assert!(refused.is_empty(), "{refused:?}");
    for item in done {
        let info = support::complete(&owner, item.job.unwrap()).await;
        let source = links.iter().find(|l| l.id == item.link).unwrap();
        assert_eq!(
            info.route.as_ref().map(|r| r.account),
            source
                .host
                .as_ref()
                .is_some_and(
                    |h| h.as_str() == "hoster.example" || h.as_str() == "cdn.hoster.example"
                )
                .then_some(account)
        );
    }
    assert_eq!(provider.unrestricts(), 2);
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(owner.handle().execute(command, ALL).await.unwrap(), first);
    assert_eq!(provider.unrestricts(), 2);
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    assert!(listed(&owner).await.is_empty());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_requires_provider_permission_and_explicit_direct_never_discovers_hosts() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    add_account(&owner, provider).await;
    let links = collect(&owner, &[ORIGINAL]).await;
    let error = owner
        .handle()
        .execute(
            envelope(&owner, automatic_command(&links)),
            &[Permission::Submit],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert!(error.message.as_str().contains("Direct"));
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 0);
    let result = owner
        .handle()
        .execute(
            envelope(&owner, start_command(&links, None)),
            &[Permission::Submit],
        )
        .await
        .unwrap();
    let (done, refused) = applied(result);
    assert!(refused.is_empty());
    support::complete(&owner, done[0].job.unwrap()).await;
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(provider.unrestricts(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_capability_outage_keeps_links_and_caches_unknown_briefly() {
    let env = env();
    let provider = &env.server.provider;
    let owner = env.start().await;
    add_account(&owner, provider).await;
    *provider.host_error.lock().unwrap() =
        Some(ProviderError::Unavailable("fixture outage".into()));
    let links = collect(&owner, &[ORIGINAL, "https://other.example/unmatched"]).await;
    for _ in 0..2 {
        let result = owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap();
        let (done, refused) = applied(result);
        assert!(done.is_empty());
        assert_eq!(refused.len(), 2);
        assert!(
            refused
                .iter()
                .all(|r| r.code == ErrorCode::Busy && r.message.as_str().contains("Direct"))
        );
    }
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.unrestricts(), 0);
    assert!(env.server.host.requests().is_empty());
    assert_eq!(listed(&owner).await.len(), 2);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_unusable_accounts_refuse_matching_links_and_disabled_is_opt_out() {
    for fault in ["invalid", "expired", "missing_credential", "locked"] {
        let env = env();
        let provider = &env.server.provider;
        env.server.host.serve(body(KIB));
        let mut owner = env.start().await;
        let account = add_account(&owner, provider).await;
        match fault {
            "invalid" => {
                database(&env)
                    .execute("UPDATE accounts SET status = 'invalid_credentials'", [])
                    .unwrap();
            }
            "expired" => {
                database(&env)
                    .execute("UPDATE accounts SET expires_at = 1", [])
                    .unwrap();
            }
            "missing_credential" => {
                database(&env)
                    .execute("DELETE FROM account_credentials", [])
                    .unwrap();
            }
            "locked" => {
                shutdown(owner).await;
                std::fs::remove_file(env.credential_key.as_ref().unwrap()).unwrap();
                owner = env.start().await;
            }
            _ => unreachable!(),
        }
        let links = collect(&owner, &[ORIGINAL, "https://other.example/file/unmatched"]).await;
        let result = owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap();
        let (done, refused) = applied(result);
        assert_eq!(done.len(), 1, "{fault}: {refused:?}");
        assert_eq!(refused.len(), 1, "{fault}");
        assert!(refused[0].message.as_str().contains("Accounts"));
        assert!(
            support::complete(&owner, done[0].job.unwrap())
                .await
                .route
                .is_none()
        );
        assert_eq!(provider.unrestricts(), 0);
        let revision = account_revision(&owner, account).await;
        owner
            .handle()
            .execute(
                envelope(
                    &owner,
                    Command::SetAccountEnabled {
                        account,
                        expected_revision: revision,
                        enabled: false,
                    },
                ),
                ALL,
            )
            .await
            .unwrap();
        let result = owner
            .handle()
            .execute(
                envelope(&owner, automatic_command(&listed(&owner).await)),
                ALL,
            )
            .await
            .unwrap();
        let (done, refused) = applied(result);
        assert_eq!(done.len(), 1);
        assert!(refused.is_empty());
        assert!(
            support::complete(&owner, done[0].job.unwrap())
                .await
                .route
                .is_none()
        );
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_provider_failure_never_tries_the_original_host() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(KIB));
    let owner = env.start().await;
    add_account(&owner, provider).await;
    provider.answer(Answer::Refuse(
        FailureCode::UnsupportedCapability,
        "fixture refusal",
        None,
    ));
    let links = collect(&owner, &[ORIGINAL]).await;
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap(),
    );
    assert!(refused.is_empty());
    let info = failed(&owner, done[0].job.unwrap()).await;
    assert!(info.route.is_some());
    assert_eq!(provider.unrestricts(), 1);
    assert!(host.requests().is_empty());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_discovery_is_single_flight_bounded_and_does_not_block_pause() {
    let env = env();
    let (provider, host) = (&env.server.provider, &env.server.host);
    host.serve(body(8 * KIB));
    host.change(|file| file.stall_at = Some(KIB));
    let owner = env.start().await;
    add_account(&owner, provider).await;
    let direct = start_one(&owner, None).await;
    let running = wait_for(&owner, direct, |j| j.stage == JobStage::Transferring).await;
    provider.hosts_wait.store(true, Ordering::SeqCst);
    let links = collect(&owner, &[ORIGINAL]).await;
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let handle = owner.handle().clone();
        let request = envelope(&owner, automatic_command(&links));
        tasks.push(tokio::spawn(
            async move { handle.execute(request, ALL).await },
        ));
    }
    while provider.hosts.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    // Let all four queued starts arrive before testing the admission bound.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let overflow = owner
        .handle()
        .execute(envelope(&owner, automatic_command(&links)), ALL)
        .await
        .unwrap_err();
    assert_eq!(overflow.code, ErrorCode::Busy);
    tokio::time::timeout(
        Duration::from_secs(2),
        owner.handle().execute(
            envelope(
                &owner,
                Command::Pause {
                    job: direct,
                    expected_revision: running.revision,
                },
            ),
            ALL,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    wait_for(&owner, direct, |j| j.stage == JobStage::Paused).await;
    // Account state may change while discovery runs; completion must use it.
    let account = match owner.handle().query(Query::Accounts {}, ALL).await.unwrap() {
        QueryResult::Accounts(a) => a.accounts[0].id,
        _ => unreachable!(),
    };
    owner
        .handle()
        .execute(
            envelope(
                &owner,
                Command::SetAccountEnabled {
                    account,
                    expected_revision: account_revision(&owner, account).await,
                    enabled: false,
                },
            ),
            ALL,
        )
        .await
        .unwrap();
    provider.hosts_gate.add_permits(1);
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(provider.unrestricts(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_unknown_large_source_id_is_not_found_and_does_not_start() {
    let env = env();
    let owner = env.start().await;
    let command = Command::StartLinks {
        account: None,
        routing: None,
        links: LinkRefs::try_from(vec![LinkRef {
            link: wayfetch_protocol::domain::id::SourceLinkId::from_u64(u64::MAX).unwrap(),
            revision: wayfetch_protocol::domain::version::Revision::INITIAL,
        }])
        .unwrap(),
    };
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, command), ALL)
            .await
            .unwrap(),
    );
    assert!(done.is_empty());
    assert_eq!(refused[0].code, ErrorCode::NotFound);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_discovery_panic_is_unknown_and_never_strands_starts() {
    let env = env();
    let provider = &env.server.provider;
    let owner = env.start().await;
    add_account(&owner, provider).await;
    provider.hosts_panic.store(true, Ordering::SeqCst);
    let links = collect(&owner, &[ORIGINAL]).await;
    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            owner
                .handle()
                .execute(envelope(&owner, automatic_command(&links)), ALL),
        )
        .await
        .unwrap()
        .unwrap();
        let (done, refused) = applied(result);
        assert!(done.is_empty());
        assert_eq!(refused[0].code, ErrorCode::Busy);
    }
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    assert_eq!(
        owner.handle().readiness(),
        wayfetch_engine::Readiness::Ready
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_routing_chooses_the_lowest_usable_account_id() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let first = add_account(&owner, provider).await;
    *provider.user.lock().unwrap() = "user-2".into();
    let second = add_account(&owner, provider).await;
    assert!(first < second);
    // Both account profiles are authenticated. Keep the first fixture
    // credential current for its eventual unrestrict call.
    *provider.current.lock().unwrap() = 1;
    let links = collect(&owner, &[ORIGINAL]).await;
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap(),
    );
    assert!(refused.is_empty());
    assert_eq!(
        support::complete(&owner, done[0].job.unwrap())
            .await
            .route
            .unwrap()
            .account,
        first
    );
    database(&env)
        .execute(
            "UPDATE accounts SET status = 'invalid_credentials' WHERE id = ?1",
            [i64::try_from(first.get()).unwrap()],
        )
        .unwrap();
    *provider.current.lock().unwrap() = 2;
    let links = collect(&owner, &["https://hoster.example/second"]).await;
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap(),
    );
    assert!(refused.is_empty());
    assert_eq!(
        support::complete(&owner, done[0].job.unwrap())
            .await
            .route
            .unwrap()
            .account,
        second
    );
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_discovery_shutdown_reaps_a_waiting_provider() {
    let env = env();
    let provider = &env.server.provider;
    let owner = env.start().await;
    add_account(&owner, provider).await;
    provider.hosts_wait.store(true, Ordering::SeqCst);
    let links = collect(&owner, &[ORIGINAL]).await;
    let handle = owner.handle().clone();
    let command = envelope(&owner, automatic_command(&links));
    let task = tokio::spawn(async move { handle.execute(command, ALL).await });
    while provider.hosts.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    shutdown(owner).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        ErrorCode::ShuttingDown
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_native_links_without_usable_accounts_stay_collected_without_network() {
    for fault in [
        "missing",
        "disabled",
        "invalid",
        "expired",
        "missing_credential",
        "locked",
    ] {
        let env = env();
        let provider = &env.server.provider;
        env.server.host.serve(body(KIB));
        let mut owner = env.start().await;
        if fault != "missing" {
            let account = add_account(&owner, provider).await;
            match fault {
                "disabled" => {
                    owner
                        .handle()
                        .execute(
                            envelope(
                                &owner,
                                Command::SetAccountEnabled {
                                    account,
                                    expected_revision: account_revision(&owner, account).await,
                                    enabled: false,
                                },
                            ),
                            ALL,
                        )
                        .await
                        .unwrap();
                }
                "invalid" => {
                    database(&env)
                        .execute("UPDATE accounts SET status = 'invalid_credentials'", [])
                        .unwrap();
                }
                "expired" => {
                    database(&env)
                        .execute("UPDATE accounts SET expires_at = 1", [])
                        .unwrap();
                }
                "missing_credential" => {
                    database(&env)
                        .execute("DELETE FROM account_credentials", [])
                        .unwrap();
                }
                "locked" => {
                    shutdown(owner).await;
                    std::fs::remove_file(env.credential_key.as_ref().unwrap()).unwrap();
                    owner = env.start().await;
                }
                _ => unreachable!(),
            }
        }
        let links = collect(&owner, &[NATIVE]).await;
        let request = envelope(&owner, automatic_command(&links));
        let first = owner.handle().execute(request.clone(), ALL).await.unwrap();
        let (done, refused) = applied(first.clone());
        assert!(done.is_empty(), "{fault}");
        assert_eq!(refused.len(), 1, "{fault}");
        assert_eq!(refused[0].code, ErrorCode::NotApplicable);
        assert!(
            refused[0].message.as_str().contains("Sign in"),
            "{fault}: {refused:?}"
        );
        assert!(
            refused[0]
                .message
                .as_str()
                .contains("Settings > Accounts & services")
        );
        assert_eq!(
            owner.handle().execute(request, ALL).await.unwrap(),
            first,
            "replay"
        );
        assert_eq!(listed(&owner).await.len(), 1);
        assert!(support::jobs(&owner).await.is_empty());
        assert_eq!(
            provider.hosts.load(Ordering::SeqCst),
            0,
            "pure native recognition"
        );
        assert_eq!(provider.unrestricts(), 0);
        assert!(env.server.host.requests().is_empty());
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_native_refusal_does_not_hold_up_unmatched_direct_links() {
    let env = env();
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let links = collect(&owner, &[NATIVE, "https://other.example/ordinary.bin"]).await;
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap(),
    );
    assert_eq!(refused.len(), 1);
    assert_eq!(done.len(), 1);
    assert!(
        support::complete(&owner, done[0].job.unwrap())
            .await
            .route
            .is_none()
    );
    assert_eq!(
        listed(&owner).await[0].host.as_ref().unwrap().as_str(),
        "provider.example"
    );
    assert_eq!(env.server.provider.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(env.server.provider.unrestricts(), 0);
    assert!(
        env.server
            .host
            .requests()
            .iter()
            .all(|(url, _)| url != NATIVE)
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_native_links_use_the_owning_account_even_with_unknown_host_metadata() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    *provider.host_error.lock().unwrap() =
        Some(ProviderError::Unavailable("fixture outage".into()));
    let links = collect(&owner, &[ORIGINAL]).await;
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap(),
    );
    assert!(done.is_empty());
    assert_eq!(refused.len(), 1);
    assert_eq!(provider.hosts.load(Ordering::SeqCst), 1);
    let links = collect(&owner, &[NATIVE]).await;
    let native = links
        .into_iter()
        .filter(|l| {
            l.host
                .as_ref()
                .is_some_and(|h| h.as_str() == "provider.example")
        })
        .collect::<Vec<_>>();
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&native)), ALL)
            .await
            .unwrap(),
    );
    assert!(refused.is_empty(), "{refused:?}");
    let job = support::complete(&owner, done[0].job.unwrap()).await;
    assert_eq!(job.route.unwrap().account, account);
    assert_eq!(
        provider.hosts.load(Ordering::SeqCst),
        1,
        "no native catalogue lookup"
    );
    assert_eq!(provider.unrestricts(), 1);
    assert!(
        env.server
            .host
            .requests()
            .iter()
            .all(|(url, _)| url != NATIVE)
    );
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn automatic_native_links_cannot_be_claimed_by_another_provider_account() {
    let mut env = env();
    let mut other = Provider::new();
    Arc::get_mut(&mut other).unwrap().service = ProviderId::new("other-debrid").unwrap();
    *other.domains.lock().unwrap() = vec!["provider.example".into()];
    env.providers.push(other.clone());
    let owner = env.start().await;
    add_account(&owner, &other).await;
    let links = collect(&owner, &[NATIVE]).await;
    let (done, refused) = applied(
        owner
            .handle()
            .execute(envelope(&owner, automatic_command(&links)), ALL)
            .await
            .unwrap(),
    );
    assert!(done.is_empty());
    assert_eq!(refused.len(), 1);
    assert!(refused[0].message.as_str().contains(SERVICE));
    assert!(support::jobs(&owner).await.is_empty());
    assert_eq!(other.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(other.unrestricts(), 0);
    assert_eq!(env.server.provider.hosts.load(Ordering::SeqCst), 0);
    assert!(env.server.host.requests().is_empty());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_links_keep_explicit_direct_and_immediate_submit_behavior() {
    use wayfetch_protocol::command::{InputUrl, SubmitItems};
    let env = env();
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let links = collect(&owner, &[NATIVE]).await;
    let direct = start(&owner, &links, None).await.unwrap()[0];
    assert!(support::complete(&owner, direct).await.route.is_none());
    let result = owner
        .handle()
        .execute(
            envelope(
                &owner,
                Command::Submit {
                    items: SubmitItems::try_from(vec![InputUrl::parse(NATIVE).unwrap()]).unwrap(),
                    package: None,
                },
            ),
            &[Permission::Submit],
        )
        .await
        .unwrap();
    assert!(matches!(result, CommandResult::Submitted { .. }));
    let submitted = support::jobs(&owner)
        .await
        .into_iter()
        .find(|j| j.id != direct)
        .unwrap();
    assert!(
        support::complete(&owner, submitted.id)
            .await
            .route
            .is_none()
    );
    assert_eq!(env.server.provider.hosts.load(Ordering::SeqCst), 0);
    assert_eq!(env.server.provider.unrestricts(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn native_starts_preserve_the_ordinary_host_catalogue_cache() {
    let env = env();
    let provider = &env.server.provider;
    env.server.host.serve(body(KIB));
    let owner = env.start().await;
    let account = add_account(&owner, provider).await;
    for (url, lookups) in [(NATIVE, 0), (ORIGINAL, 1), (NATIVE, 1), (ORIGINAL, 1)] {
        let links = collect(&owner, &[url]).await;
        let (done, refused) = applied(
            owner
                .handle()
                .execute(envelope(&owner, automatic_command(&links)), ALL)
                .await
                .unwrap(),
        );
        assert!(refused.is_empty(), "{refused:?}");
        let job = support::complete(&owner, done[0].job.unwrap()).await;
        assert_eq!(job.route.unwrap().account, account);
        assert_eq!(provider.hosts.load(Ordering::SeqCst), lookups);
    }
    assert_eq!(provider.unrestricts(), 4);
    shutdown(owner).await;
}

/// Answers link checks: every link is a web page linking to an unrelated
/// PDF until `files` is set, then a file.
#[derive(Default)]
struct Pages {
    files: AtomicBool,
}

impl Inspector for Pages {
    fn inspect<'a>(
        &'a self,
        request: &'a InspectRequest,
    ) -> BoxFuture<'a, Result<Inspection, InspectError>> {
        let page =
            !self.files.load(Ordering::SeqCst) && request.url.host_str() == Some("hoster.example");
        let answer = if page {
            Inspection {
                status: 200,
                final_url: Some(request.url.clone()),
                content_type: Some("text/html".into()),
                page: Some(Bytes::from_static(
                    b"<a href=\"https://docs.example/terms.pdf\">Terms</a>",
                )),
                ..Inspection::default()
            }
        } else {
            Inspection {
                status: 200,
                content_type: Some("application/octet-stream".into()),
                length: Some(1),
                ..Inspection::default()
            }
        };
        Box::pin(async move { Ok(answer) })
    }
}

/// The Links list once no check is waiting or running.
async fn checked(owner: &EngineOwner) -> Vec<CollectedLink> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let query = Query::ListLinks {
            after: None,
            page_size: PageSize::DEFAULT,
        };
        let QueryResult::Links(page) = owner.handle().query(query, ALL).await.unwrap() else {
            panic!()
        };
        if page.checking == 0 {
            return page.items;
        }
        assert!(tokio::time::Instant::now() < deadline, "{page:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Checks `links` again.
async fn check_again(owner: &EngineOwner, links: &[&CollectedLink]) {
    let refs = links
        .iter()
        .map(|l| LinkRef {
            link: l.id,
            revision: l.revision,
        })
        .collect::<Vec<_>>();
    let check = Command::CheckLinks {
        links: LinkRefs::try_from(refs).unwrap(),
    };
    owner
        .handle()
        .execute(envelope(owner, check), ALL)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hoster_page_starts_through_an_account_or_once_it_is_a_file() {
    let mut env = env();
    let pages = Arc::new(Pages::default());
    env.inspector = Some(pages.clone());
    let (provider, host) = (&env.server.provider, &env.server.host);
    let data = body(4 * KIB);
    host.serve(data.clone());
    *provider.size.lock().unwrap() = Some(data.len() as u64);
    let owner = env.start().await;
    // Collected before any account: the hoster answers its own check.
    collect(&owner, &[ORIGINAL, "https://hoster.example/file/later"]).await;
    let links = checked(&owner).await;
    let [hoster, later, pdf] = &links[..] else {
        panic!("{links:?}")
    };
    assert_eq!((hoster.check, hoster.found), (LinkCheck::Page, Some(1)));
    assert_eq!(later.check, LinkCheck::Page);
    assert_eq!(hoster.checked_via, None);
    assert_eq!(pdf.found_on.as_deref(), Some(ORIGINAL));
    assert_eq!(provider.availabilities.load(Ordering::SeqCst), 0);

    // Once an account would download them, checking again asks its
    // provider, which finds the file, without generating anything.
    let account = add_account(&owner, provider).await;
    check_again(&owner, &[hoster, pdf]).await;
    let links = checked(&owner).await;
    let [hoster, _, pdf] = &links[..] else {
        panic!("{links:?}")
    };
    assert_eq!(hoster.check, LinkCheck::Online);
    assert_eq!(hoster.checked_via.as_ref(), Some(&provider.service));
    assert_eq!(hoster.name_hint.as_deref(), Some("Provider Name.bin"));
    assert_eq!(hoster.size, Some(data.len() as u64));
    // A link on a host no account supports is still asked directly.
    assert_eq!(
        (pdf.check, pdf.checked_via.as_ref()),
        (LinkCheck::Online, None)
    );
    assert_eq!(provider.availabilities.load(Ordering::SeqCst), 1);
    assert_eq!(provider.unrestricts(), 0);

    // Its own server still serves a web page, so Direct still refuses it.
    let command = start_command(std::slice::from_ref(hoster), None);
    let CommandResult::LinksApplied { applied, refused } = owner
        .handle()
        .execute(envelope(&owner, command), ALL)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(applied.is_empty());
    assert!(
        refused[0].message.as_str().contains("web page"),
        "{refused:?}"
    );

    // Through the account, the provider resolves it to its file.
    let [id] = start(&owner, std::slice::from_ref(hoster), Some(account))
        .await
        .unwrap()[..]
    else {
        panic!()
    };
    support::complete(&owner, id).await;
    assert_eq!(env.published("Provider Name.bin"), data);

    // What the provider cannot tell is unknown, still through it, and
    // what it says is gone is offline.
    *provider.availability.lock().unwrap() = Some(Availability::Unknown(
        CheckReason::ServerError,
        "The provider cannot reach this host right now.".into(),
    ));
    check_again(&owner, &[later]).await;
    let links = checked(&owner).await;
    let later = links.iter().find(|l| l.id == later.id).unwrap();
    assert_eq!(later.check, LinkCheck::Unknown);
    assert_eq!(later.reason, Some(CheckReason::ServerError));
    assert_eq!(later.checked_via.as_ref(), Some(&provider.service));
    *provider.availability.lock().unwrap() = Some(Availability::Offline("Gone.".into()));
    check_again(&owner, &[later]).await;
    let links = checked(&owner).await;
    let later = links.iter().find(|l| l.id == later.id).unwrap();
    assert_eq!(
        (later.check, later.reason),
        (LinkCheck::Offline, Some(CheckReason::NotFound))
    );

    // With the account disabled, the hoster answers again: a page checked
    // again as a file starts directly.
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
    pages.files.store(true, Ordering::SeqCst);
    check_again(&owner, &[later]).await;
    let links = checked(&owner).await;
    let later = links.iter().find(|l| l.id == later.id).unwrap();
    assert_eq!(
        (later.check, later.checked_via.as_ref()),
        (LinkCheck::Online, None)
    );
    assert_eq!(provider.availabilities.load(Ordering::SeqCst), 3);
    start(&owner, std::slice::from_ref(later), None)
        .await
        .unwrap();
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_provider_fails_only_the_check() {
    for hosts in [true, false] {
        let mut env = env();
        env.inspector = Some(Arc::new(Pages::default()));
        let provider = &env.server.provider;
        let owner = env.start().await;
        add_account(&owner, provider).await;
        provider.hosts_panic.store(hosts, Ordering::SeqCst);
        provider.availability_panic.store(!hosts, Ordering::SeqCst);
        collect(&owner, &[ORIGINAL]).await;
        let links = checked(&owner).await;
        let link = links.iter().find(|l| l.found_on.is_none()).unwrap();
        if hosts {
            // Its supported hosts cannot be read: the link's own server
            // answers.
            assert_eq!(
                (link.check, link.checked_via.as_ref()),
                (LinkCheck::Page, None)
            );
        } else {
            // Its check panics: unknown through it.
            assert_eq!(
                (link.check, link.checked_via.as_ref()),
                (LinkCheck::Unknown, Some(&provider.service))
            );
            assert_eq!(provider.availabilities.load(Ordering::SeqCst), 1);
        }
        assert_eq!(
            owner.handle().readiness(),
            wayfetch_engine::Readiness::Ready
        );
        shutdown(owner).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_asks_to_slow_down_is_not_asked_again_for_a_while() {
    let mut env = env();
    env.inspector = Some(Arc::new(Pages::default()));
    let provider = &env.server.provider;
    *provider.availability.lock().unwrap() = Some(Availability::Unknown(
        CheckReason::RateLimited,
        "Slow down.".into(),
    ));
    let owner = env.start().await;
    add_account(&owner, provider).await;
    let many: Vec<String> = (0..12)
        .map(|n| format!("https://hoster.example/file/{n}"))
        .collect();
    collect(&owner, &many.iter().map(String::as_str).collect::<Vec<_>>()).await;
    let links = checked(&owner).await;
    let typed: Vec<_> = links.iter().filter(|l| l.found_on.is_none()).collect();
    assert_eq!(typed.len(), 12);
    for link in &typed {
        assert_eq!(
            (link.check, link.reason, link.checked_via.as_ref()),
            (
                LinkCheck::Unknown,
                Some(CheckReason::RateLimited),
                Some(&provider.service)
            )
        );
    }
    // Only the checks already under way when it first asked reached it.
    assert!(provider.availabilities.load(Ordering::SeqCst) <= 2);
    shutdown(owner).await;
}
