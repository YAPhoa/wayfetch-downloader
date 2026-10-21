//! Provider accounts through the public engine API, with an in-process
//! provider (ADR-24). Real-Debrid's own client is tested in
//! `wayfetch-adapters`.

mod support;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, envelope, shutdown};
use tokio::sync::Semaphore;
use url::Url;
use wayfetch_engine::EngineOwner;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, Checked, DeviceAuthorization, PollOutcome, Profile, ProviderError,
};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_protocol::account::{
    AccountAlias, AccountPlan, AccountRef, AccountStatus, AccountSummary, Accounts, Authorization,
    AuthorizationMethod, AuthorizationState, ServiceSupport, VaultState,
};
use wayfetch_protocol::domain::id::AccountId;
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::name::ProviderId;
use wayfetch_protocol::domain::time::UtcTimestamp;
use wayfetch_protocol::domain::version::Revision;
use wayfetch_protocol::{
    Command, CommandResult, ErrorCode, Permission, ProtocolError, Query, QueryResult,
};

const SERVICE: &str = "synthetic-debrid";
/// What the provider hands out that grants access: never shown or stored
/// in the clear.
const CREDENTIAL: &str = "synthetic-credential-7f3a9c";
const DEVICE_SECRET: &str = "synthetic-device-secret-41b2";

/// A provider whose answers the test releases one by one.
struct Fake {
    service: ProviderId,
    begins: AtomicUsize,
    polls: AtomicUsize,
    checks: AtomicUsize,
    /// Permits to answer a poll with approval; polls wait for one.
    approvals: Semaphore,
    /// Permits to answer a check, when checks are held.
    check_permits: Semaphore,
    hold_checks: Mutex<bool>,
    /// Credentials issued so far, numbered.
    issued: AtomicUsize,
    /// What the next checks answer, in order; then success.
    check_results: Mutex<VecDeque<Checked>>,
    /// The credentials checks were given.
    checked: Mutex<Vec<String>>,
    user: Mutex<Profile>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            service: ProviderId::new(SERVICE).unwrap(),
            begins: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
            approvals: Semaphore::new(0),
            check_permits: Semaphore::new(0),
            hold_checks: Mutex::new(false),
            issued: AtomicUsize::new(0),
            check_results: Mutex::new(VecDeque::new()),
            checked: Mutex::new(Vec::new()),
            user: Mutex::new(profile("4242", "synthetic-user")),
        })
    }

    fn approve(&self) {
        self.approvals.add_permits(1);
    }

    fn calls(&self) -> usize {
        self.begins.load(Ordering::SeqCst)
            + self.polls.load(Ordering::SeqCst)
            + self.checks.load(Ordering::SeqCst)
    }

    fn hold_checks(&self, hold: bool) {
        *self.hold_checks.lock().unwrap() = hold;
    }

    fn last_checked(&self) -> String {
        self.checked.lock().unwrap().last().cloned().unwrap()
    }
}

fn profile(id: &str, name: &str) -> Profile {
    Profile {
        user_id: Some(id.into()),
        username: Some(name.into()),
        plan: Some(AccountPlan::Premium),
        expires_at: Some(UtcTimestamp::from_unix_millis(4_102_444_800_000)),
    }
}

impl AccountProvider for Fake {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn support(&self) -> ServiceSupport {
        ServiceSupport {
            service: self.service.clone(),
            method: AuthorizationMethod::DeviceCode,
            downloads: false,
        }
    }

    fn begin(&self) -> BoxFuture<'_, Result<DeviceAuthorization, ProviderError>> {
        Box::pin(async move {
            self.begins.fetch_add(1, Ordering::SeqCst);
            Ok(DeviceAuthorization {
                user_code: "ABCD-1234".into(),
                verification_url: Url::parse("https://provider.example/device").unwrap(),
                expires_in: Duration::from_secs(600),
                interval: Duration::from_secs(1),
                pending: Secret::from_text(DEVICE_SECRET),
            })
        })
    }

    fn poll<'a>(
        &'a self,
        pending: &'a Secret,
    ) -> BoxFuture<'a, Result<PollOutcome, ProviderError>> {
        Box::pin(async move {
            self.polls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(pending.expose_text(), Some(DEVICE_SECRET));
            self.approvals.acquire().await.unwrap().forget();
            let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(PollOutcome::Authorized {
                credential: Secret::from_text(&format!("{CREDENTIAL}-{n}")),
                profile: self.user.lock().unwrap().clone(),
            })
        })
    }

    fn check<'a>(&'a self, credential: &'a Secret) -> BoxFuture<'a, Checked> {
        Box::pin(async move {
            self.checks.fetch_add(1, Ordering::SeqCst);
            let text = credential.expose_text().unwrap().to_owned();
            self.checked.lock().unwrap().push(text);
            let hold = *self.hold_checks.lock().unwrap();
            if hold {
                self.check_permits.acquire().await.unwrap().forget();
            }
            let next = self.check_results.lock().unwrap().pop_front();
            next.unwrap_or_else(|| Checked {
                result: Ok(self.user.lock().unwrap().clone()),
                refreshed: None,
            })
        })
    }
}

/// A transport serving one small file at every URL.
struct Files;

struct Body(Option<Bytes>);

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
        Box::pin(async move { Ok(self.0.take()) })
    }
}

impl Transport for Files {
    fn open<'a>(
        &'a self,
        _request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        Box::pin(async move {
            let data = Bytes::from_static(b"account-free download");
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
                body: Box::new(Body(Some(data))),
            })
        })
    }
}

type Env = support::Env<Arc<Fake>>;

fn key_path(env: &Env) -> PathBuf {
    env.state.parent().unwrap().join("credential.key")
}

/// An environment whose engines know the fake provider and keep their
/// credential key next to (not in) the state directory.
fn env() -> Env {
    let fake = Fake::new();
    let mut env = Env::new(fake.clone(), Arc::new(Files));
    env.providers.push(fake);
    env.credential_key = Some(key_path(&env));
    env
}

fn service() -> ProviderId {
    ProviderId::new(SERVICE).unwrap()
}

async fn accounts(owner: &EngineOwner) -> Accounts {
    match owner.handle().query(Query::Accounts {}, ALL).await.unwrap() {
        QueryResult::Accounts(accounts) => accounts,
        other => panic!("{other:?}"),
    }
}

async fn account(owner: &EngineOwner, id: AccountId) -> AccountSummary {
    accounts(owner)
        .await
        .accounts
        .into_iter()
        .find(|a| a.id == id)
        .expect("listed")
}

async fn authorization(
    owner: &EngineOwner,
    id: wayfetch_protocol::account::AuthorizationId,
) -> Authorization {
    match owner
        .handle()
        .query(Query::Authorization { id }, ALL)
        .await
        .unwrap()
    {
        QueryResult::Authorization(authorization) => authorization,
        other => panic!("{other:?}"),
    }
}

fn authorize_command(account: Option<AccountRef>, alias: Option<&str>) -> Command {
    Command::AuthorizeAccount {
        service: service(),
        account,
        alias: alias.map(|a| AccountAlias::new(a).unwrap()),
    }
}

async fn start_authorization(owner: &EngineOwner, command: Command) -> Authorization {
    match owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap()
    {
        CommandResult::AuthorizationStarted(authorization) => authorization,
        other => panic!("{other:?}"),
    }
}

/// Polls an authorization until it is no longer pending.
async fn ended(
    owner: &EngineOwner,
    id: wayfetch_protocol::account::AuthorizationId,
) -> Authorization {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let current = authorization(owner, id).await;
        if current.state != AuthorizationState::Pending {
            return current;
        }
        assert!(tokio::time::Instant::now() < deadline, "{current:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Adds an account through the whole sign-in.
async fn add(owner: &EngineOwner, fake: &Fake) -> AccountSummary {
    let started = start_authorization(owner, authorize_command(None, Some("Main"))).await;
    fake.approve();
    let done = ended(owner, started.id).await;
    assert_eq!(done.state, AuthorizationState::Completed, "{done:?}");
    account(owner, done.account.unwrap()).await
}

async fn refresh(owner: &EngineOwner, id: AccountId) -> Result<AccountSummary, ProtocolError> {
    owner.handle().refresh_account(id, ALL).await
}

async fn set_enabled(
    owner: &EngineOwner,
    id: AccountId,
    expected_revision: Revision,
    enabled: bool,
) -> Result<CommandResult, ProtocolError> {
    let command = Command::SetAccountEnabled {
        account: id,
        expected_revision,
        enabled,
    };
    owner.handle().execute(envelope(owner, command), ALL).await
}

async fn remove(
    owner: &EngineOwner,
    id: AccountId,
    expected_revision: Revision,
) -> Result<CommandResult, ProtocolError> {
    let command = Command::RemoveAccount {
        account: id,
        expected_revision,
    };
    owner.handle().execute(envelope(owner, command), ALL).await
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Every file in the state directory, the database's WAL and shared
/// memory included.
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

fn assert_no_secret_in_state(env: &Env) {
    let files = state_files(&env.state);
    assert!(
        files.iter().any(|(p, _)| p.ends_with("state.sqlite3")),
        "{:?}",
        files.iter().map(|(p, _)| p).collect::<Vec<_>>()
    );
    for (path, bytes) in files {
        for secret in [CREDENTIAL, DEVICE_SECRET] {
            assert!(!contains(&bytes, secret), "{secret} in {}", path.display());
        }
    }
}

fn assert_no_secret(text: &str) {
    for secret in [CREDENTIAL, DEVICE_SECRET] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_survive_restarts_with_sealed_credentials() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let listed = accounts(&owner).await;
    assert_eq!(listed.vault, VaultState::Ready);
    assert!(listed.accounts.is_empty());
    assert_eq!(listed.services.len(), 1);
    assert!(!listed.services[0].downloads);
    assert!(
        !key_path(&env).exists(),
        "the key is created with the first account"
    );

    let started = start_authorization(&owner, authorize_command(None, Some("Main"))).await;
    assert_eq!(started.state, AuthorizationState::Pending);
    assert_eq!(started.user_code.as_deref(), Some("ABCD-1234"));
    assert_no_secret(&serde_json::to_string(&started).unwrap());
    fake.approve();
    let done = ended(&owner, started.id).await;
    assert_eq!(done.state, AuthorizationState::Completed);
    assert_eq!(done.user_code, None, "the code is not shown once used");
    let added = account(&owner, done.account.unwrap()).await;
    assert_eq!(added.status, AccountStatus::Authenticated);
    assert_eq!(added.username.as_deref(), Some("synthetic-user"));
    assert_eq!(added.alias.as_ref().map(|a| a.as_str()), Some("Main"));
    assert_eq!(added.plan, Some(AccountPlan::Premium));
    assert!(added.enabled);
    assert_no_secret(&serde_json::to_string(&accounts(&owner).await).unwrap());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(key_path(&env))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Neither while the engine runs, with the WAL not checkpointed, nor
    // after it stopped.
    let wal = env.state.join("state.sqlite3-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0);
    assert_no_secret_in_state(&env);
    shutdown(owner).await;
    assert_no_secret_in_state(&env);

    // After a restart the account is listed as it was, and its credential
    // opens with the same key.
    let owner = env.start().await;
    let after = account(&owner, added.id).await;
    assert_eq!(after, added);
    let checked = refresh(&owner, added.id).await.unwrap();
    assert_eq!(checked.status, AccountStatus::Authenticated);
    assert_eq!(fake.last_checked(), format!("{CREDENTIAL}-1"));
    shutdown(owner).await;
    assert_no_secret_in_state(&env);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_wrong_or_exposed_key_locks_accounts_but_not_downloads() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    shutdown(owner).await;
    let key = std::fs::read(key_path(&env)).unwrap();

    let other_key = format!("{}\n", "ab".repeat(32));
    type BreakKey = Box<dyn Fn(&Path)>;
    let cases: [(&str, BreakKey); 3] = [
        ("missing", Box::new(|p| std::fs::remove_file(p).unwrap())),
        (
            "wrong",
            Box::new(move |p| std::fs::write(p, &other_key).unwrap()),
        ),
        (
            "exposed",
            Box::new(|p| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
                }
                #[cfg(not(unix))]
                std::fs::remove_file(p).unwrap();
            }),
        ),
    ];
    for (case, break_key) in cases {
        let path = key_path(&env);
        break_key(&path);
        let owner = env.start().await;
        let listed = accounts(&owner).await;
        assert_eq!(listed.vault, VaultState::Locked, "{case}");
        assert!(listed.vault_problem.is_some(), "{case}");
        // Still listed, and still described as last checked.
        assert_eq!(listed.accounts.len(), 1, "{case}");
        let calls = fake.calls();
        let err = refresh(&owner, added.id).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotApplicable, "{case}: {err:?}");
        let err = owner
            .handle()
            .execute(envelope(&owner, authorize_command(None, None)), ALL)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotApplicable, "{case}: {err:?}");
        assert_eq!(fake.calls(), calls, "{case}: no provider call while locked");
        if case == "missing" {
            assert!(!path.exists(), "a missing key is never replaced");
        }

        // Downloads that need no account work as before.
        let [job] = support::submit(&owner, ["https://files.example/a.bin".to_owned()]).await[..]
        else {
            panic!()
        };
        let done = support::complete(&owner, job).await;
        assert_eq!(done.stage, JobStage::Complete);
        shutdown(owner).await;

        // The real key unlocks the vault again.
        let _ = std::fs::remove_file(&path);
        write_key(&path, &key);
        let owner = env.start().await;
        assert_eq!(accounts(&owner).await.vault, VaultState::Ready, "{case}");
        shutdown(owner).await;
    }
}

fn write_key(path: &Path, key: &[u8]) {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).unwrap().write_all(key).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_locked_vault_still_disables_and_removes_and_then_starts_over() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    shutdown(owner).await;
    std::fs::remove_file(key_path(&env)).unwrap();

    let owner = env.start().await;
    assert_eq!(accounts(&owner).await.vault, VaultState::Locked);
    let CommandResult::AccountUpdated { revision, .. } =
        set_enabled(&owner, added.id, added.revision, false)
            .await
            .unwrap()
    else {
        panic!()
    };
    remove(&owner, added.id, revision).await.unwrap();
    // No account is left to protect, so a new key may start the vault.
    let listed = accounts(&owner).await;
    assert_eq!(listed.vault, VaultState::Ready);
    assert!(listed.accounts.is_empty());
    let again = add(&owner, &fake).await;
    assert_eq!(again.status, AccountStatus::Authenticated);
    assert!(key_path(&env).exists());
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_configured_key_accounts_cannot_be_stored() {
    let mut env = env();
    env.credential_key = None;
    let fake = env.server.clone();
    let owner = env.start().await;
    let listed = accounts(&owner).await;
    assert_eq!(listed.vault, VaultState::NotConfigured);
    let err = owner
        .handle()
        .execute(envelope(&owner, authorize_command(None, None)), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    assert_eq!(fake.calls(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_credentials_fail_safely() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    shutdown(owner).await;

    let db = rusqlite::Connection::open(env.state.join("state.sqlite3")).unwrap();
    let mut sealed: Vec<u8> = db
        .query_row("SELECT sealed FROM account_credentials", [], |r| r.get(0))
        .unwrap();
    sealed[20] ^= 1;
    db.execute("UPDATE account_credentials SET sealed = ?1", [sealed])
        .unwrap();
    drop(db);

    let owner = env.start().await;
    let calls = fake.calls();
    let checked = refresh(&owner, added.id).await.unwrap();
    assert_eq!(checked.status, AccountStatus::InvalidCredentials);
    assert!(checked.problem.is_some());
    assert_eq!(fake.calls(), calls, "nothing was sent");
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_and_stale_revisions() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;

    // A retried sign-in answers with the same authorization, and once it
    // completed, with the account it added: one account either way.
    let sign_in = envelope(&owner, authorize_command(None, None));
    let first = owner.handle().execute(sign_in.clone(), ALL).await.unwrap();
    let second = owner.handle().execute(sign_in.clone(), ALL).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(fake.begins.load(Ordering::SeqCst), 1);
    let CommandResult::AuthorizationStarted(started) = first else {
        panic!()
    };
    fake.approve();
    let done = ended(&owner, started.id).await;
    let id = done.account.unwrap();
    let replay = owner.handle().execute(sign_in.clone(), ALL).await.unwrap();
    assert!(
        matches!(replay, CommandResult::AccountAuthorized { account, .. } if account == id),
        "{replay:?}"
    );
    let mut changed = sign_in.clone();
    changed.command = authorize_command(None, Some("Other"));
    let err = owner.handle().execute(changed, ALL).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::IdempotencyConflict);
    assert_eq!(accounts(&owner).await.accounts.len(), 1);

    // The same person signed in again as a new account is refused.
    let started = start_authorization(&owner, authorize_command(None, None)).await;
    fake.approve();
    let failed = ended(&owner, started.id).await;
    assert_eq!(failed.state, AuthorizationState::Failed, "{failed:?}");
    assert_eq!(accounts(&owner).await.accounts.len(), 1);

    // Revisions.
    let current = account(&owner, id).await;
    let stale = current.revision;
    let disable = envelope(
        &owner,
        Command::SetAccountEnabled {
            account: id,
            expected_revision: stale,
            enabled: false,
        },
    );
    let CommandResult::AccountUpdated {
        revision,
        changed: true,
        ..
    } = owner.handle().execute(disable.clone(), ALL).await.unwrap()
    else {
        panic!()
    };
    assert_ne!(revision, stale);
    // The retry replays; a new request at the old revision conflicts.
    assert_eq!(
        owner.handle().execute(disable, ALL).await.unwrap(),
        CommandResult::AccountUpdated {
            account: id,
            revision,
            changed: true
        }
    );
    let err = set_enabled(&owner, id, stale, true).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict);
    assert_eq!(err.current_revision, Some(revision));
    let err = remove(&owner, id, stale).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::RevisionConflict);

    // A disabled account is kept but not used.
    let calls = fake.calls();
    let err = refresh(&owner, id).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    let err = owner
        .handle()
        .execute(
            envelope(
                &owner,
                authorize_command(
                    Some(AccountRef {
                        account: id,
                        revision,
                    }),
                    None,
                ),
            ),
            ALL,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    assert_eq!(fake.calls(), calls);
    let disabled = account(&owner, id).await;
    assert!(!disabled.enabled);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replacing_credentials_wins_over_a_late_check() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;

    // A check that will report refreshed credentials, held at the
    // provider.
    fake.hold_checks(true);
    fake.check_results.lock().unwrap().push_back(Checked {
        result: Ok(profile("4242", "stale-name")),
        refreshed: Some(Secret::from_text("obsolete-refreshed-credential")),
    });
    let late = {
        let handle = owner.handle();
        tokio::spawn(async move { handle.refresh_account(added.id, ALL).await })
    };
    while fake.checks.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Signing in again replaces the credentials meanwhile.
    let started = start_authorization(
        &owner,
        authorize_command(
            Some(AccountRef {
                account: added.id,
                revision: added.revision,
            }),
            None,
        ),
    )
    .await;
    assert_eq!(started.account, Some(added.id));
    fake.approve();
    let done = ended(&owner, started.id).await;
    assert_eq!(done.state, AuthorizationState::Completed, "{done:?}");
    let replaced = account(&owner, added.id).await;
    assert!(replaced.revision > added.revision);

    // The late check's result is not stored.
    fake.check_permits.add_permits(1);
    let answered = late.await.unwrap().unwrap();
    assert_eq!(answered.username.as_deref(), Some("synthetic-user"));
    assert_eq!(account(&owner, added.id).await, replaced);
    fake.hold_checks(false);
    refresh(&owner, added.id).await.unwrap();
    assert_eq!(fake.last_checked(), format!("{CREDENTIAL}-2"));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removal_wins_over_a_late_check_or_sign_in() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;

    fake.hold_checks(true);
    let late = {
        let handle = owner.handle();
        tokio::spawn(async move { handle.refresh_account(added.id, ALL).await })
    };
    while fake.checks.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = start_authorization(
        &owner,
        authorize_command(
            Some(AccountRef {
                account: added.id,
                revision: added.revision,
            }),
            None,
        ),
    )
    .await;
    remove(&owner, added.id, added.revision).await.unwrap();
    // The sign-in for the removed account stops at once.
    let stopped = authorization(&owner, started.id).await;
    assert_eq!(stopped.state, AuthorizationState::Cancelled);
    fake.approve();
    fake.check_permits.add_permits(1);
    let err = late.await.unwrap().unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let listed = accounts(&owner).await;
    assert!(listed.accounts.is_empty(), "{listed:?}");
    let db = env.state.join("state.sqlite3");
    shutdown(owner).await;
    let db = rusqlite::Connection::open(db).unwrap();
    let left: i64 = db
        .query_row("SELECT count(*) FROM account_credentials", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_checks_share_one_provider_call() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    let before = fake.checks.load(Ordering::SeqCst);

    fake.hold_checks(true);
    let mut waiting = Vec::new();
    for _ in 0..5 {
        let handle = owner.handle();
        waiting.push(tokio::spawn(async move {
            handle.refresh_account(added.id, ALL).await
        }));
    }
    while fake.checks.load(Ordering::SeqCst) == before {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    fake.check_permits.add_permits(1);
    for waiter in waiting {
        let summary = waiter.await.unwrap().unwrap();
        assert_eq!(summary.status, AccountStatus::Authenticated);
    }
    assert_eq!(fake.checks.load(Ordering::SeqCst), before + 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn check_failures_are_told_apart() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    fake.check_results.lock().unwrap().extend([
        Checked::failed(ProviderError::Unavailable("connection reset".into())),
        Checked::failed(ProviderError::InvalidCredentials(
            "the provider no longer accepts this sign-in".into(),
        )),
    ]);
    let checked = refresh(&owner, added.id).await.unwrap();
    assert_eq!(checked.status, AccountStatus::Unavailable);
    let checked = refresh(&owner, added.id).await.unwrap();
    assert_eq!(checked.status, AccountStatus::InvalidCredentials);
    let checked = refresh(&owner, added.id).await.unwrap();
    assert_eq!(checked.status, AccountStatus::Authenticated);
    assert_eq!(checked.problem, None);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshed_credentials_are_kept_when_the_check_then_fails() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    const ROTATED: &str = "synthetic-rotated-credential";
    fake.check_results.lock().unwrap().push_back(Checked {
        result: Err(ProviderError::Unavailable("service unavailable".into())),
        refreshed: Some(Secret::from_text(ROTATED)),
    });
    let checked = refresh(&owner, added.id).await.unwrap();
    assert_eq!(checked.status, AccountStatus::Unavailable);
    // The next check uses the refreshed credentials, sealed like any other.
    refresh(&owner, added.id).await.unwrap();
    assert_eq!(fake.last_checked(), ROTATED);
    shutdown(owner).await;
    for (path, bytes) in state_files(&env.state) {
        assert!(!contains(&bytes, ROTATED), "{} holds it", path.display());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_must_confirm_which_account_it_reached() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    let again = |account: &AccountSummary| {
        authorize_command(
            Some(AccountRef {
                account: account.id,
                revision: account.revision,
            }),
            None,
        )
    };

    // The provider could not say who signed in: nothing is replaced.
    *fake.user.lock().unwrap() = Profile {
        user_id: None,
        ..Profile::default()
    };
    let started = start_authorization(&owner, again(&added)).await;
    fake.approve();
    let done = ended(&owner, started.id).await;
    assert_eq!(done.state, AuthorizationState::Failed, "{done:?}");
    assert!(done.problem.unwrap().as_str().contains("did not confirm"));
    assert_eq!(account(&owner, added.id).await, added);

    // Nor is a new account added.
    let started = start_authorization(&owner, authorize_command(None, None)).await;
    fake.approve();
    assert_eq!(
        ended(&owner, started.id).await.state,
        AuthorizationState::Failed
    );

    // Another user at the provider cannot take over the account.
    *fake.user.lock().unwrap() = profile("9999", "someone-else");
    let started = start_authorization(&owner, again(&added)).await;
    fake.approve();
    let done = ended(&owner, started.id).await;
    assert_eq!(done.state, AuthorizationState::Failed, "{done:?}");
    let kept = account(&owner, added.id).await;
    assert_eq!(kept, added);
    assert_eq!(kept.username.as_deref(), Some("synthetic-user"));
    assert_eq!(accounts(&owner).await.accounts.len(), 1);
    refresh(&owner, added.id).await.unwrap();
    assert_eq!(fake.last_checked(), format!("{CREDENTIAL}-1"));
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_ins_are_bounded_and_cancellable() {
    let env = env();
    let owner = env.start().await;
    let mut started = Vec::new();
    for _ in 0..4 {
        started.push(start_authorization(&owner, authorize_command(None, None)).await);
    }
    let err = owner
        .handle()
        .execute(envelope(&owner, authorize_command(None, None)), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Overloaded);
    let cancelled = owner
        .handle()
        .cancel_authorization(started[0].id, ALL)
        .await
        .unwrap();
    assert_eq!(cancelled.state, AuthorizationState::Cancelled);
    assert_eq!(cancelled.user_code, None);
    start_authorization(&owner, authorize_command(None, None)).await;
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_accounts_permission_manages_accounts() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    let added = add(&owner, &fake).await;
    let calls = fake.calls();
    let others = [
        Permission::Read,
        Permission::Submit,
        Permission::Control,
        Permission::Sources,
    ];
    let handle = owner.handle();
    fn denied<T: std::fmt::Debug>(r: Result<T, ProtocolError>) {
        assert_eq!(r.unwrap_err().code, ErrorCode::PermissionDenied);
    }
    denied(handle.query(Query::Accounts {}, &others).await);
    denied(
        handle
            .execute(envelope(&owner, authorize_command(None, None)), &others)
            .await,
    );
    denied(
        handle
            .execute(
                envelope(
                    &owner,
                    Command::RemoveAccount {
                        account: added.id,
                        expected_revision: added.revision,
                    },
                ),
                &others,
            )
            .await,
    );
    denied(handle.refresh_account(added.id, &others).await);
    denied(
        handle
            .cancel_authorization(
                wayfetch_protocol::account::AuthorizationId::generate(),
                &others,
            )
            .await,
    );
    assert_eq!(fake.calls(), calls);
    assert_eq!(accounts(&owner).await.accounts.len(), 1);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_never_contacts_the_provider() {
    let env = env();
    let fake = env.server.clone();
    let owner = env.start().await;
    add(&owner, &fake).await;
    let calls = fake.calls();
    for _ in 0..5 {
        accounts(&owner).await;
    }
    assert_eq!(fake.calls(), calls);
    shutdown(owner).await;
}
