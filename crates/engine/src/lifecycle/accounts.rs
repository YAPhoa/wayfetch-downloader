//! The coordinator's account work: authorizations waiting for a person's
//! approval, single-flight status checks, and the vault (architecture
//! brief sections 7.3 and 8.3, ADR-24).
//!
//! Provider calls run in engine tasks of their own and never inside a
//! database transaction; their results come back to the coordinator as
//! internal messages, which store them only if the account's credentials
//! are still the generation the work started from. Everything is bounded:
//! at most [`MAX_PENDING`] authorizations start or wait at once, finished
//! ones are kept briefly for clients to read, at most [`MAX_CHECKS`]
//! accounts are checked at once, each by one provider call however many
//! clients asked, and every provider call has a deadline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use wayfetch_domain::id::AccountId;
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::version::Generation;
use wayfetch_protocol::account::{
    AccountStatus, AccountSummary, Accounts, Authorization, AuthorizationId, AuthorizationState,
    MAX_ACCOUNTS,
};
use wayfetch_protocol::command::IdempotencyKey;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, ProtocolError, QueryResult,
};

use super::resolve::{Lane, LaneWork};
use super::{Coordinator, Internal, now};
use crate::accounts::Secret;
use crate::accounts::provider::{
    AccountProvider, Checked, DeviceAuthorization, PollOutcome, Profile, ProviderError,
};
use crate::accounts::vault::{Keys, Vault};
use crate::store::{CheckResult, StoredCredential};

/// Authorizations starting or waiting for approval at once.
pub(super) const MAX_PENDING: usize = 4;
/// Ended authorizations kept for clients to read.
const MAX_ENDED: usize = 16;
/// How long an ended authorization is kept.
const ENDED_FOR: Duration = Duration::from_secs(15 * 60);
/// Accounts checked at once.
pub(super) const MAX_CHECKS: usize = 8;
/// Requests waiting for one account's check.
const MAX_WAITERS: usize = 16;
/// Deadline for one provider call, refreshes included.
pub(super) const PROVIDER_DEADLINE: Duration = Duration::from_secs(60);
/// The longest an authorization waits, whatever the provider says.
const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// Bounds on the provider's polling interval.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// What a provider's request to slow down adds (RFC 8628 section 3.5).
const SLOW_DOWN: Duration = Duration::from_secs(5);
/// Failed polls in a row before an authorization gives up.
const MAX_POLL_FAILURES: u32 = 5;
/// Deadline for reading or creating the key file.
const KEY_DEADLINE: Duration = Duration::from_secs(10);

type Reply<T> = oneshot::Sender<Result<T, ProtocolError>>;

/// What an authorization signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    /// A new account.
    New,
    /// An existing account, whose credentials were this generation.
    Replace {
        account: AccountId,
        generation: Generation,
    },
}

impl Target {
    fn account(self) -> Option<AccountId> {
        match self {
            Self::New => None,
            Self::Replace { account, .. } => Some(account),
        }
    }
}

struct Flow {
    view: Authorization,
    envelope: Box<CommandEnvelope>,
    target: Target,
    cancel: CancellationToken,
    ended: Option<Instant>,
}

struct Starting {
    envelope: Box<CommandEnvelope>,
    waiters: Vec<Reply<CommandResult>>,
}

/// The coordinator's account state.
pub(super) struct AccountService {
    providers: Vec<Arc<dyn AccountProvider>>,
    pub(super) vault: Vault,
    key_path: Option<PathBuf>,
    flows: HashMap<AuthorizationId, Flow>,
    by_key: HashMap<IdempotencyKey, AuthorizationId>,
    starting: HashMap<IdempotencyKey, Starting>,
    checks: HashMap<AccountId, Vec<Reply<AccountSummary>>>,
    /// Each account's provider calls, one at a time.
    pub(super) lanes: HashMap<AccountId, Lane>,
}

impl AccountService {
    pub(super) fn new(
        providers: Vec<Arc<dyn AccountProvider>>,
        vault: Vault,
        key_path: Option<PathBuf>,
    ) -> Self {
        Self {
            providers,
            vault,
            key_path,
            flows: HashMap::new(),
            by_key: HashMap::new(),
            starting: HashMap::new(),
            checks: HashMap::new(),
            lanes: HashMap::new(),
        }
    }

    /// Installed adapters, including providers with no signed-in account.
    pub(super) fn providers(&self) -> &[Arc<dyn AccountProvider>] {
        &self.providers
    }

    pub(super) fn provider(&self, service: &ProviderId) -> Option<Arc<dyn AccountProvider>> {
        self.providers
            .iter()
            .find(|p| p.service() == service)
            .cloned()
    }

    fn pending(&self) -> usize {
        self.starting.len()
            + self
                .flows
                .values()
                .filter(|f| f.view.state == AuthorizationState::Pending)
                .count()
    }

    fn pending_new(&self) -> usize {
        self.starting
            .values()
            .filter(|s| {
                matches!(
                    &s.envelope.command,
                    Command::AuthorizeAccount { account: None, .. }
                )
            })
            .count()
            + self
                .flows
                .values()
                .filter(|f| f.view.state == AuthorizationState::Pending && f.target == Target::New)
                .count()
    }

    /// Ends an authorization that is still waiting.
    fn end(&mut self, id: AuthorizationId, state: AuthorizationState, problem: Option<&str>) {
        if let Some(flow) = self.flows.get_mut(&id)
            && flow.view.state == AuthorizationState::Pending
        {
            flow.cancel.cancel();
            flow.view.state = state;
            flow.view.user_code = None;
            flow.view.verification_url = None;
            flow.view.problem = problem.map(DiagnosticText::new);
            flow.ended = Some(Instant::now());
        }
        self.prune();
    }

    /// Cancels the authorizations waiting to sign `account` in again.
    pub(super) fn overtake(&mut self, account: AccountId, why: &str) {
        let ids: Vec<_> = self
            .flows
            .iter()
            .filter(|(_, f)| f.target.account() == Some(account))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.end(id, AuthorizationState::Cancelled, Some(why));
        }
    }

    /// Forgets ended authorizations past their time or beyond the bound.
    fn prune(&mut self) {
        let mut ended: Vec<_> = self
            .flows
            .iter()
            .filter_map(|(id, f)| f.ended.map(|at| (at, *id)))
            .collect();
        ended.sort_by_key(|(at, _)| *at);
        let excess = ended.len().saturating_sub(MAX_ENDED);
        for (i, (at, id)) in ended.into_iter().enumerate() {
            if (i < excess || at.elapsed() >= ENDED_FOR)
                && let Some(flow) = self.flows.remove(&id)
            {
                self.by_key.remove(&flow.envelope.idempotency_key);
            }
        }
    }

    /// The vault after the last account was removed: its key check is gone,
    /// so the next account starts it again.
    pub(super) fn emptied(&mut self) {
        self.vault = match &self.key_path {
            Some(path) => Vault::Uninitialized(path.clone()),
            None => Vault::NotConfigured,
        };
    }
}

fn provider_error(service: &ProviderId, err: ProviderError) -> ProtocolError {
    match err {
        ProviderError::Unavailable(why) => ProtocolError::new(
            ErrorCode::Busy,
            &format!("{service} could not be reached ({why}); try again"),
        ),
        ProviderError::InvalidCredentials(why) | ProviderError::Malformed(why) => {
            ProtocolError::new(
                ErrorCode::NotApplicable,
                &format!("{service} refused: {why}"),
            )
        }
    }
}

fn not_pending(state: AuthorizationState) -> bool {
    state != AuthorizationState::Pending
}

impl Coordinator {
    /// Answers [`wayfetch_protocol::Query::Accounts`]. Never contacts a
    /// provider.
    pub(super) async fn list_accounts(&mut self, reply: Reply<QueryResult>) {
        let rows = self.with_store(|store| store.accounts()).await;
        let (vault, vault_problem) = self.accounts.vault.state();
        let services = self
            .accounts
            .providers
            .iter()
            .map(|p| p.support())
            .collect();
        let _ = reply.send(rows.map_err(ProtocolError::from).map(|rows| {
            QueryResult::Accounts(Accounts {
                vault,
                vault_problem,
                services,
                accounts: rows.iter().map(|r| r.summary()).collect(),
            })
        }));
    }

    /// Answers [`wayfetch_protocol::Query::Authorization`].
    pub(super) fn authorization(&mut self, id: AuthorizationId, reply: Reply<QueryResult>) {
        self.accounts.prune();
        let found = self.accounts.flows.get(&id).map(|f| f.view.clone());
        let _ = reply.send(
            found
                .map(QueryResult::Authorization)
                .ok_or_else(unknown_authorization),
        );
    }

    /// Cancels an authorization that is still waiting, and answers with it.
    pub(super) fn cancel_authorization(
        &mut self,
        id: AuthorizationId,
        reply: Reply<Authorization>,
    ) {
        self.accounts
            .end(id, AuthorizationState::Cancelled, Some("cancelled"));
        let found = self.accounts.flows.get(&id).map(|f| f.view.clone());
        let _ = reply.send(found.ok_or_else(unknown_authorization));
    }

    /// Runs [`Command::AuthorizeAccount`]: replays a recorded result,
    /// answers a retry with its authorization, or checks the request and
    /// starts one with the provider in a task of its own.
    pub(super) async fn authorize(
        &mut self,
        envelope: Box<CommandEnvelope>,
        reply: Reply<CommandResult>,
    ) {
        let Command::AuthorizeAccount {
            service, account, ..
        } = &envelope.command
        else {
            return;
        };
        let (service, account) = (service.clone(), *account);
        let key = envelope.idempotency_key;
        let instance = self.identity.instance;
        let replay = envelope.clone();
        match self
            .with_store(move |store| store.replayed_command(&replay, instance))
            .await
        {
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
            Ok(Some(result)) => {
                let _ = reply.send(Ok(result));
                return;
            }
            Ok(None) => {}
        }
        let conflict = || {
            ProtocolError::new(
                ErrorCode::IdempotencyConflict,
                "idempotency key was used for a different command",
            )
        };
        if let Some(flow) = self
            .accounts
            .by_key
            .get(&key)
            .and_then(|id| self.accounts.flows.get(id))
        {
            let _ = reply.send(if flow.envelope.command == envelope.command {
                Ok(CommandResult::AuthorizationStarted(flow.view.clone()))
            } else {
                Err(conflict())
            });
            return;
        }
        if let Some(starting) = self.accounts.starting.get_mut(&key) {
            if starting.envelope.command != envelope.command {
                let _ = reply.send(Err(conflict()));
            } else if starting.waiters.len() >= MAX_WAITERS {
                let _ = reply.send(Err(overloaded()));
            } else {
                starting.waiters.push(reply);
            }
            return;
        }
        let Some(provider) = self.accounts.provider(&service) else {
            let _ = reply.send(Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "Wayfetch cannot sign in to accounts of this service yet",
            )));
            return;
        };
        let target = match self.authorization_target(&service, account).await {
            Ok(target) => target,
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        };
        if self.accounts.pending() >= MAX_PENDING {
            let _ = reply.send(Err(ProtocolError::new(
                ErrorCode::Overloaded,
                "too many sign-ins are waiting; finish or cancel one first",
            )));
            return;
        }
        if let Err(e) = self.unlock_vault().await {
            let _ = reply.send(Err(e));
            return;
        }
        // A newer sign-in of the same account wins.
        if let Some(account) = target.account() {
            self.accounts.overtake(
                account,
                "the account was signed in again from another request",
            );
        }
        self.accounts.starting.insert(
            key,
            Starting {
                envelope,
                waiters: vec![reply],
            },
        );
        let client = self.client.clone();
        let cancel = self.supervisor.cancel.clone();
        self.supervisor.spawn(async move {
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                result = tokio::time::timeout(PROVIDER_DEADLINE, provider.begin()) => result
                    .unwrap_or_else(|_| Err(ProviderError::Unavailable("no answer in time".into()))),
            };
            let _ = client
                .0
                .send(Internal::AccountBegun {
                    key,
                    target,
                    result,
                })
                .await;
        });
    }

    /// Checks what an authorization would sign in: an enabled account of
    /// the same service at the revision the client saw, or room for one
    /// more account.
    async fn authorization_target(
        &mut self,
        service: &ProviderId,
        account: Option<wayfetch_protocol::account::AccountRef>,
    ) -> Result<Target, ProtocolError> {
        let Some(reference) = account else {
            let count = self.with_store(|store| store.accounts()).await?.len();
            if count + self.accounts.pending_new() >= MAX_ACCOUNTS {
                return Err(ProtocolError::new(
                    ErrorCode::LimitExceeded,
                    "no more accounts can be added",
                ));
            }
            return Ok(Target::New);
        };
        let id = reference.account;
        let row = self.with_store(move |store| store.account(id)).await?;
        if &row.service != service {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "the account belongs to another service",
            ));
        }
        if row.revision != reference.revision {
            return Err(ProtocolError {
                current_revision: Some(row.revision),
                ..ProtocolError::new(
                    ErrorCode::RevisionConflict,
                    "the account changed since it was read",
                )
            });
        }
        if !row.enabled {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "enable the account before signing it in again",
            ));
        }
        Ok(Target::Replace {
            account: id,
            generation: row.generation,
        })
    }

    /// Makes the vault ready for a new credential: for a vault that never
    /// stored a key, reads the configured key file, creating it if
    /// missing, and stores its check. Never replaces a stored key.
    async fn unlock_vault(&mut self) -> Result<Arc<Keys>, ProtocolError> {
        let Vault::Uninitialized(path) = &self.accounts.vault else {
            return self.accounts.vault.keys();
        };
        let path = path.clone();
        let guard = self.supervisor.guard();
        let read = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            Vault::initialize(&path)
        });
        let keys = match tokio::time::timeout(KEY_DEADLINE, read).await {
            Ok(Ok(Ok(keys))) => keys,
            Ok(Ok(Err(problem))) => {
                return Err(ProtocolError::new(ErrorCode::NotApplicable, &problem));
            }
            Ok(Err(_)) | Err(_) => {
                return Err(ProtocolError::new(
                    ErrorCode::NotApplicable,
                    "the credential key file could not be read in time",
                ));
            }
        };
        let check = keys.check();
        self.with_store(move |store| store.store_key_check(&check, now()))
            .await?;
        let keys = Arc::new(keys);
        self.accounts.vault = Vault::Ready(keys.clone());
        Ok(keys)
    }

    /// The provider started an authorization, or could not: answers the
    /// requests waiting for it and, once started, polls it in a task.
    pub(super) fn begun(
        &mut self,
        key: IdempotencyKey,
        target: Target,
        result: Result<DeviceAuthorization, ProviderError>,
    ) {
        let Some(starting) = self.accounts.starting.remove(&key) else {
            return;
        };
        let Command::AuthorizeAccount { service, .. } = &starting.envelope.command else {
            return;
        };
        let service = service.clone();
        let device = match result {
            Ok(device) => device,
            Err(e) => {
                let e = provider_error(&service, e);
                for waiter in starting.waiters {
                    let _ = waiter.send(Err(e.clone()));
                }
                return;
            }
        };
        let Some(provider) = self.accounts.provider(&service) else {
            return;
        };
        let lifetime = device.expires_in.min(MAX_LIFETIME);
        let id = AuthorizationId::generate();
        let view = Authorization {
            id,
            service,
            state: AuthorizationState::Pending,
            user_code: Some(device.user_code),
            verification_url: Some(device.verification_url.into()),
            expires_at: now().checked_add(lifetime).unwrap_or_else(now),
            account: target.account(),
            problem: None,
        };
        let cancel = self.supervisor.cancel.child_token();
        for waiter in starting.waiters {
            let _ = waiter.send(Ok(CommandResult::AuthorizationStarted(view.clone())));
        }
        self.accounts.by_key.insert(key, id);
        self.accounts.flows.insert(
            id,
            Flow {
                view,
                envelope: starting.envelope,
                target,
                cancel: cancel.clone(),
                ended: None,
            },
        );
        let client = self.client.clone();
        let interval = device.interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
        let pending = device.pending;
        self.supervisor.spawn(async move {
            let ended = poll(&*provider, &pending, interval, lifetime, &cancel).await;
            let message = match ended {
                Polled::Authorized(credential, profile) => Internal::AccountAuthorized {
                    flow: id,
                    credential,
                    profile,
                },
                Polled::Ended(state, problem) => Internal::AccountFlowEnded {
                    flow: id,
                    state,
                    problem,
                },
                Polled::Stopped => return,
            };
            let _ = client.0.send(message).await;
        });
    }

    /// The person approved an authorization: stores the account, or its
    /// new credentials, unless the authorization was cancelled or
    /// overtaken meanwhile, in which case the credentials are dropped.
    pub(super) async fn authorized(
        &mut self,
        id: AuthorizationId,
        credential: Secret,
        profile: Profile,
    ) {
        let Some(flow) = self.accounts.flows.get(&id) else {
            return;
        };
        if not_pending(flow.view.state) {
            return;
        }
        let (envelope, target, service) = (
            flow.envelope.clone(),
            flow.target,
            flow.view.service.clone(),
        );
        let stored = self
            .accounts
            .vault
            .keys()
            .and_then(|keys| seal(&keys, &service, &credential));
        let stored = match stored {
            Ok(stored) => stored,
            Err(e) => {
                self.accounts
                    .end(id, AuthorizationState::Failed, Some(e.message.as_str()));
                return;
            }
        };
        // Without the provider's ID for the account, the engine cannot tell
        // a new account from one already added, or confirm a replacement.
        if profile.user_id.is_none() {
            self.accounts.end(
                id,
                AuthorizationState::Failed,
                Some("the service did not confirm which account signed in; try again"),
            );
            return;
        }
        let instance = self.identity.instance;
        let result = self
            .with_store(move |store| match target {
                Target::New => store.add_account(&envelope, instance, &stored, &profile, now()),
                Target::Replace {
                    account,
                    generation,
                } => store.replace_credentials(
                    &envelope,
                    instance,
                    account,
                    generation,
                    &stored,
                    &profile,
                    now(),
                ),
            })
            .await;
        match result {
            Ok(CommandResult::AccountAuthorized { account, .. }) => {
                let flow = self.accounts.flows.get_mut(&id).expect("checked above");
                flow.view.account = Some(account);
                flow.view.state = AuthorizationState::Completed;
                flow.view.user_code = None;
                flow.view.verification_url = None;
                flow.ended = Some(Instant::now());
                self.accounts.prune();
            }
            Ok(_) => self.accounts.end(id, AuthorizationState::Failed, None),
            Err(e) => {
                // Removed, signed in again, or disabled meanwhile; any other
                // refusal, such as another user at the provider, failed.
                let overtaken = matches!(e.code, ErrorCode::NotFound | ErrorCode::RevisionConflict);
                let state = if overtaken {
                    AuthorizationState::Cancelled
                } else {
                    AuthorizationState::Failed
                };
                self.accounts.end(id, state, Some(e.message.as_str()));
            }
        }
    }

    /// An authorization ended without approval.
    pub(super) fn flow_ended(
        &mut self,
        id: AuthorizationId,
        state: AuthorizationState,
        problem: Option<String>,
    ) {
        self.accounts.end(id, state, problem.as_deref());
    }

    /// Checks an account with its provider, refreshing its credentials if
    /// needed. Requests for an account already being checked wait for that
    /// check instead of starting another; a check waits for the account's
    /// other provider calls to finish, so it reads their credentials.
    pub(super) async fn refresh_account(
        &mut self,
        account: AccountId,
        reply: Reply<AccountSummary>,
    ) {
        if let Err(e) = self.accounts.vault.keys() {
            let _ = reply.send(Err(e));
            return;
        }
        if let Some(waiters) = self.accounts.checks.get_mut(&account) {
            if waiters.len() >= MAX_WAITERS {
                let _ = reply.send(Err(overloaded()));
            } else {
                waiters.push(reply);
            }
            return;
        }
        if self.accounts.checks.len() >= MAX_CHECKS {
            let _ = reply.send(Err(overloaded()));
            return;
        }
        let row = match self.with_store(move |store| store.account(account)).await {
            Ok(row) => row,
            Err(e) => {
                let _ = reply.send(Err(e.into()));
                return;
            }
        };
        if let Err(e) = self.checkable(&row) {
            let _ = reply.send(Err(e));
            return;
        }
        self.accounts.checks.insert(account, vec![reply]);
        self.enqueue(account, LaneWork::Check).await;
    }

    /// Whether an account can be checked now.
    fn checkable(&self, row: &crate::store::AccountRow) -> Result<(), ProtocolError> {
        if !row.enabled {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "the account is disabled; enable it to check it",
            ));
        }
        if self.accounts.provider(&row.service).is_none() {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "Wayfetch cannot check accounts of this service",
            ));
        }
        Ok(())
    }

    /// Starts the account's queued check with its credentials as they are
    /// now, in a task of its own. Returns whether it started; if not, its
    /// waiters were answered.
    pub(super) async fn start_check(&mut self, account: AccountId) -> bool {
        let fail = |this: &mut Self, e: ProtocolError| {
            for waiter in this.accounts.checks.remove(&account).unwrap_or_default() {
                let _ = waiter.send(Err(e.clone()));
            }
            false
        };
        let keys = match self.accounts.vault.keys() {
            Ok(keys) => keys,
            Err(e) => return fail(self, e),
        };
        let loaded = self
            .with_store(move |store| store.account_credential(account))
            .await;
        let (row, stored) = match loaded {
            Ok(loaded) => loaded,
            Err(e) => return fail(self, e.into()),
        };
        if let Err(e) = self.checkable(&row) {
            return fail(self, e);
        }
        let Some(provider) = self.accounts.provider(&row.service) else {
            return false;
        };
        let generation = row.generation;
        let credential = match keys.open(&stored.reference, &row.service, &stored.sealed) {
            Ok(credential) => credential,
            Err(e) => {
                let failed = CheckResult::Failed {
                    status: AccountStatus::InvalidCredentials,
                    problem: format!("{e}; sign in again to replace them"),
                    refreshed: None,
                };
                let row = self
                    .with_store(move |store| {
                        store.record_check(account, generation, &failed, now())
                    })
                    .await;
                let summary = answer(row);
                for waiter in self.accounts.checks.remove(&account).unwrap_or_default() {
                    let _ = waiter.send(summary.clone());
                }
                return false;
            }
        };
        let client = self.client.clone();
        let cancel = self.supervisor.cancel.clone();
        self.supervisor.spawn(async move {
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                result = tokio::time::timeout(PROVIDER_DEADLINE, provider.check(&credential)) =>
                    result.unwrap_or_else(|_| {
                        Checked::failed(ProviderError::Unavailable("no answer in time".into()))
                    }),
            };
            let _ = client
                .0
                .send(Internal::AccountChecked {
                    account,
                    generation,
                    result,
                })
                .await;
        });
        true
    }

    /// A check ended: stores what it found, if the credentials it read are
    /// still current, and answers everyone waiting with the account as it
    /// is now.
    pub(super) async fn checked(
        &mut self,
        account: AccountId,
        generation: Generation,
        checked: Checked,
    ) {
        let waiters = self.accounts.checks.remove(&account).unwrap_or_default();
        // Refreshed credentials are kept whatever the check then found.
        let refreshed = match checked.refreshed {
            None => Ok(None),
            Some(credential) => self
                .with_store(move |store| store.account(account))
                .await
                .map_err(Into::into)
                .and_then(|row| {
                    let keys = self.accounts.vault.keys()?;
                    seal(&keys, &row.service, &credential).map(Some)
                }),
        };
        let found = match (refreshed, checked.result) {
            (Err(e), _) => CheckResult::Failed {
                status: AccountStatus::Unavailable,
                problem: e.message.into(),
                refreshed: None,
            },
            (Ok(refreshed), Ok(profile)) => CheckResult::Authenticated { profile, refreshed },
            (Ok(refreshed), Err(ProviderError::InvalidCredentials(why))) => CheckResult::Failed {
                status: AccountStatus::InvalidCredentials,
                problem: why,
                refreshed,
            },
            (
                Ok(refreshed),
                Err(ProviderError::Unavailable(why) | ProviderError::Malformed(why)),
            ) => CheckResult::Failed {
                status: AccountStatus::Unavailable,
                problem: why,
                refreshed,
            },
        };
        let row = self
            .with_store(move |store| store.record_check(account, generation, &found, now()))
            .await;
        let summary = answer(row);
        for waiter in waiters {
            let _ = waiter.send(summary.clone());
        }
        self.lane_done(account).await;
    }

    /// After a committed account command: stops sign-ins it overtook, and
    /// resets the vault once no account is left.
    pub(super) async fn account_changed(&mut self, command: &Command) {
        match command {
            Command::RemoveAccount { account, .. } => {
                self.accounts.overtake(*account, "the account was removed");
                self.stop_routed(
                    *account,
                    "The account this download used was removed. Remove the download, then \
                     collect its link again to download it another way.",
                )
                .await;
                let empty = self
                    .with_store(|store| store.accounts().map(|rows| rows.is_empty()))
                    .await;
                if empty.unwrap_or(false) {
                    self.accounts.emptied();
                }
            }
            Command::SetAccountEnabled {
                account,
                enabled: false,
                ..
            } => {
                self.accounts.overtake(*account, "the account was disabled");
                self.stop_routed(
                    *account,
                    "The account this download uses was disabled. Enable it in Accounts, then \
                     retry.",
                )
                .await;
            }
            _ => {}
        }
    }
}

/// How polling an authorization ended.
enum Polled {
    Authorized(Secret, Profile),
    Ended(AuthorizationState, Option<String>),
    /// Cancelled: the coordinator already knows.
    Stopped,
}

/// Polls an authorization at the provider's interval until it is approved,
/// refused, expired, cancelled, or keeps failing.
async fn poll(
    provider: &dyn AccountProvider,
    pending: &Secret,
    mut interval: Duration,
    lifetime: Duration,
    cancel: &CancellationToken,
) -> Polled {
    let deadline = tokio::time::Instant::now() + lifetime;
    let mut failures = 0;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Polled::Stopped,
            () = tokio::time::sleep(interval) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Polled::Ended(AuthorizationState::Expired, None);
        }
        let outcome = tokio::select! {
            () = cancel.cancelled() => return Polled::Stopped,
            outcome = tokio::time::timeout_at(deadline, provider.poll(pending)) => outcome,
        };
        match outcome {
            Err(_) => return Polled::Ended(AuthorizationState::Expired, None),
            Ok(Ok(PollOutcome::Pending)) => failures = 0,
            Ok(Ok(PollOutcome::SlowDown)) => {
                failures = 0;
                interval = (interval + SLOW_DOWN).min(MAX_INTERVAL);
            }
            Ok(Ok(PollOutcome::Authorized {
                credential,
                profile,
            })) => return Polled::Authorized(credential, profile),
            Ok(Ok(PollOutcome::Denied(why))) => {
                return Polled::Ended(AuthorizationState::Failed, Some(why));
            }
            Ok(Ok(PollOutcome::Expired)) => {
                return Polled::Ended(AuthorizationState::Expired, None);
            }
            Ok(Err(ProviderError::Unavailable(why))) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    return Polled::Ended(
                        AuthorizationState::Failed,
                        Some(format!("the provider could not be reached ({why})")),
                    );
                }
            }
            Ok(Err(e)) => return Polled::Ended(AuthorizationState::Failed, Some(e.to_string())),
        }
    }
}

pub(super) fn seal(
    keys: &Keys,
    service: &ProviderId,
    credential: &Secret,
) -> Result<StoredCredential, ProtocolError> {
    let fail = |e: crate::accounts::vault::SealError| {
        ProtocolError::new(ErrorCode::Internal, &format!("credentials not stored: {e}"))
    };
    let reference = keys.reference().map_err(fail)?;
    let sealed = keys.seal(&reference, service, credential).map_err(fail)?;
    Ok(StoredCredential { reference, sealed })
}

fn answer(
    row: Result<Option<crate::store::AccountRow>, crate::store::StoreError>,
) -> Result<AccountSummary, ProtocolError> {
    match row {
        Ok(Some(row)) => Ok(row.summary()),
        Ok(None) => Err(ProtocolError::new(
            ErrorCode::NotFound,
            "the account was removed",
        )),
        Err(e) => Err(e.into()),
    }
}

fn unknown_authorization() -> ProtocolError {
    ProtocolError::new(ErrorCode::NotFound, "no such sign-in")
}

fn overloaded() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::Overloaded,
        "too many account requests are waiting; try again shortly",
    )
}
