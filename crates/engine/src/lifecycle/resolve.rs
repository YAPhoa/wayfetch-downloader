//! Turning a job's link into a download link through the account the user
//! chose (ADR-09, ADR-24).
//!
//! Calls to one account run one at a time. A link request is recorded as an
//! intent before it is sent, and a lost answer is never resent: the
//! provider's history is checked instead, and without clear evidence the job
//! waits for the user to confirm a repeat. Results apply only if the job,
//! its generation, and its account are unchanged.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::{AccountId, JobId};
use wayfetch_domain::job::{DesiredState, JobStage};
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::query::{FailureSummary, JobRoute};

use super::{Coordinator, Internal, now};
use crate::accounts::Secret;
use crate::accounts::provider::{GeneratedLink, Reconciled, Unrestricted, WithCredentials};
use crate::accounts::vault::Keys;
use crate::store::{Operation, OperationOutcome, OperationState};

/// Deadline for one provider call, refreshes included.
const PROVIDER_DEADLINE: Duration = super::accounts::PROVIDER_DEADLINE;
/// How much earlier than the recorded intent the provider's clock may
/// date a link it generated for it.
const CLOCK_SKEW: Duration = Duration::from_secs(10 * 60);
/// Longest generated link kept.
const MAX_LINK_BYTES: usize = 4096;
/// Longest suggested file name kept.
const MAX_NAME_BYTES: usize = 1024;

/// A download's request for its link.
pub(crate) struct ResolveRequest {
    /// The job.
    pub(crate) job: JobId,
    /// The generation asking.
    pub(crate) generation: Generation,
    /// The original link.
    pub(crate) link: Url,
    /// The operation whose link the download host refused, to be replaced.
    pub(crate) refused: Option<i64>,
    /// Where the answer goes.
    pub(crate) reply: oneshot::Sender<Resolution>,
}

/// The answer to a [`ResolveRequest`]. No answer (a dropped sender) means
/// the work is stale.
#[derive(Debug)]
pub(crate) enum Resolution {
    /// A link to download from.
    Link(ResolvedLink),
    /// Why there is none: the job fails with this.
    Failed(FailureSummary),
}

/// A link for one job generation.
#[derive(Debug)]
pub(crate) struct ResolvedLink {
    /// The operation that generated it.
    pub(crate) operation: i64,
    /// The link.
    pub(crate) link: GeneratedLink,
    /// The provider and account it is bound to.
    pub(crate) route: JobRoute,
}

/// Work waiting for an account's lane.
pub(super) enum LaneWork {
    /// The account's status check, whose waiters are in `checks`.
    Check,
    /// A link request.
    Resolve(Box<Pending>),
}

/// One account's provider calls: at most one runs.
#[derive(Default)]
pub(super) struct Lane {
    pub(super) busy: bool,
    pub(super) waiting: VecDeque<LaneWork>,
}

/// A link request waiting for, or holding, its account's lane.
pub(super) struct Pending {
    request: ResolveRequest,
    route: JobRoute,
    step: Step,
}

/// What the provider is asked.
enum Step {
    /// Generate a link, under this new intent.
    Ask(Operation),
    /// Look for the outcome of this earlier intent; then, if the user
    /// confirmed a repeat (`repeat`) and none is found, ask again.
    Reconcile { operation: Operation, repeat: bool },
}

/// What a provider task found.
pub(super) struct Resolved {
    pending: Box<Pending>,
    /// The credential generation the task read.
    generation: Generation,
    /// The account's sign-in the task read.
    sign_in: Generation,
    found: Found,
    refreshed: Option<Secret>,
}

enum Found {
    /// The answer to a request, and when it was lost, what reconciling
    /// found.
    Asked(Unrestricted, Option<Reconciled>),
    /// What looking for an earlier request's outcome found.
    Looked(Reconciled),
}

/// A generated link as sealed.
#[derive(Serialize, Deserialize)]
struct Sealed {
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    /// As `algorithm:hex`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    checksums: Vec<String>,
}

impl Drop for Sealed {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.url);
    }
}

fn failure(code: FailureCode, message: &str, retry_at: Option<UtcTimestamp>) -> FailureSummary {
    FailureSummary {
        code,
        retry_at,
        message: Some(DiagnosticText::new(message)),
    }
}

const AMBIGUOUS: &str = "The provider's answer was lost, and its record shows no link for this \
     download, so Wayfetch did not ask again. Retry and confirm to ask again; the provider may \
     then repeat the work.";

const UNSAVED: &str = "Wayfetch could not record the provider's answer, so it did not use it. \
     Retry and confirm to look for the link the provider made; the provider may then repeat the \
     work.";

const SIGNED_IN_AGAIN: &str = "The account was signed in again while Wayfetch was asking for this \
     download's link, so that answer was not used. Retry to download with the account's current \
     sign-in.";

const CLOSED: &str = "This download's provider request was closed meanwhile, so its answer was \
     not used. Retry the download.";

/// The earliest a provider may date a link generated for `op`.
fn since(op: &Operation) -> UtcTimestamp {
    UtcTimestamp::from_unix_millis(
        op.started_at
            .unix_millis()
            .saturating_sub(i64::try_from(CLOCK_SKEW.as_millis()).unwrap_or(i64::MAX)),
    )
}

/// Whether two original links may name the same file: the same host and
/// path, in any form.
fn may_be_same(a: &Url, b: &Url) -> bool {
    let key = |u: &Url| {
        (
            u.host_str().map(str::to_ascii_lowercase),
            u.path().trim_end_matches('/').to_owned(),
        )
    };
    key(a) == key(b)
}

/// Checks a link a provider generated before it is stored or used: HTTPS
/// (or plain HTTP to a loopback test server), no credentials in it, and a
/// bounded length. The provider's name and size are hints, bounded too.
fn checked_link(link: GeneratedLink) -> Option<GeneratedLink> {
    let url = &link.url;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    let secure = url.scheme() == "https" || url.scheme() == "http" && loopback;
    let ok = secure
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.as_str().len() <= MAX_LINK_BYTES;
    ok.then(|| GeneratedLink {
        filename: link.filename.filter(|n| n.len() <= MAX_NAME_BYTES),
        ..link
    })
}

fn seal_link(
    keys: &Keys,
    service: &ProviderId,
    link: &GeneratedLink,
    sign_in: Generation,
) -> Option<OperationOutcome> {
    let payload = Sealed {
        url: link.url.as_str().to_owned(),
        filename: link.filename.clone(),
        size: link.size,
        checksums: link.checksums.iter().map(ToString::to_string).collect(),
    };
    let secret = Secret::new(serde_json::to_vec(&payload).ok()?);
    let reference = keys.reference().ok()?;
    let sealed = keys.seal_result(&reference, service, &secret).ok()?;
    Some(OperationOutcome::Succeeded {
        reference,
        sealed,
        sign_in,
    })
}

fn open_link(keys: &Keys, operation: &Operation) -> Option<GeneratedLink> {
    let (reference, sealed) = operation.sealed.as_ref()?;
    let secret = keys
        .open_result(reference, &operation.service, sealed)
        .ok()?;
    let payload: Sealed = serde_json::from_slice(secret.expose()).ok()?;
    checked_link(GeneratedLink {
        url: Url::parse(&payload.url).ok()?,
        filename: payload.filename.clone(),
        size: payload.size,
        checksums: payload
            .checksums
            .iter()
            .filter_map(|d| d.parse().ok())
            .collect(),
    })
}

impl Coordinator {
    /// A download asks for its link: answers with the stored one, or
    /// queues a provider call on the account's lane.
    pub(super) async fn resolve(&mut self, request: ResolveRequest) {
        let job = request.job;
        let found = self
            .with_store(move |store| store.provider_operation(job))
            .await;
        let (route, operation) = match found {
            Ok((Some(route), operation)) => (route, operation),
            // A direct job, or a storage fault: nothing to resolve.
            _ => return,
        };
        let (keys, sign_in) = match self.usable_account(&route).await {
            Ok(usable) => usable,
            Err(failed) => {
                let _ = request.reply.send(Resolution::Failed(failed));
                return;
            }
        };
        let step = match operation {
            Some(op) if op.account != route.account => {
                // Never another account's link.
                let id = op.id;
                let _ = self
                    .with_store(move |store| store.retire_operation(id, now()))
                    .await;
                None
            }
            Some(op) if op.state == OperationState::Succeeded => {
                // Only a link from the account's current sign-in.
                if request.refused != Some(op.id)
                    && op.sign_in == Some(sign_in)
                    && let Some(link) = open_link(&keys, &op)
                {
                    let _ = request.reply.send(Resolution::Link(ResolvedLink {
                        operation: op.id,
                        link,
                        route,
                    }));
                    return;
                }
                // Refused by the download host, from an earlier sign-in,
                // or unreadable: replaced.
                let id = op.id;
                let _ = self
                    .with_store(move |store| store.retire_operation(id, now()))
                    .await;
                None
            }
            Some(op) if op.state == OperationState::Ambiguous => {
                let _ = request.reply.send(Resolution::Failed(failure(
                    FailureCode::OperationAmbiguous,
                    AMBIGUOUS,
                    None,
                )));
                return;
            }
            Some(op) => {
                let repeat = op.state == OperationState::Confirmed;
                Some(Step::Reconcile {
                    operation: op,
                    repeat,
                })
            }
            None => None,
        };
        let pending = match step {
            Some(step) => Pending {
                request,
                route,
                step,
            },
            None => match self.intend(request, route).await {
                Some(pending) => pending,
                None => return,
            },
        };
        let account = pending.route.account;
        self.enqueue(account, LaneWork::Resolve(Box::new(pending)))
            .await;
    }

    /// Records the intent of asking for `request`'s link. `None`, with the
    /// request answered or dropped, if the job moved on or storage failed.
    async fn intend(&mut self, request: ResolveRequest, route: JobRoute) -> Option<Pending> {
        let (job, generation, at) = (request.job, request.generation, now());
        let intended = route.clone();
        let operation = self
            .with_store(move |store| store.begin_operation(job, generation, &intended, at))
            .await
            .ok()?;
        Some(Pending {
            request,
            route,
            step: Step::Ask(operation),
        })
    }

    /// The vault's keys and the account's current sign-in, if `route`'s
    /// account can be used now; else the failure that says why not and
    /// what to do.
    async fn usable_account(
        &mut self,
        route: &JobRoute,
    ) -> Result<(std::sync::Arc<Keys>, Generation), FailureSummary> {
        let id = route.account;
        let row = self.with_store(move |store| store.account(id)).await;
        let row = match row {
            Ok(row) => row,
            Err(crate::store::StoreError::NotFound(_)) => {
                return Err(failure(
                    FailureCode::InvalidCredentials,
                    "The account this download uses was removed. Remove the download, then \
                     collect its link again to download it another way.",
                    None,
                ));
            }
            Err(_) => {
                return Err(failure(
                    FailureCode::InvalidCredentials,
                    "The account this download uses could not be read. Retry the download.",
                    None,
                ));
            }
        };
        if row.service != route.service {
            return Err(failure(
                FailureCode::InvalidCredentials,
                "The account this download uses belongs to another service.",
                None,
            ));
        }
        if !row.enabled {
            return Err(failure(
                FailureCode::InvalidCredentials,
                "The account this download uses is disabled. Enable it in Accounts, then retry.",
                None,
            ));
        }
        let supported = self
            .accounts
            .provider(&route.service)
            .is_some_and(|p| p.support().downloads);
        if !supported {
            return Err(failure(
                FailureCode::UnsupportedCapability,
                "Wayfetch cannot download through accounts of this service.",
                None,
            ));
        }
        let keys = self.accounts.vault.keys().map_err(|e| {
            failure(
                FailureCode::InvalidCredentials,
                &format!("{} Fix it, then retry.", e.message),
                None,
            )
        })?;
        Ok((keys, row.sign_in))
    }

    /// Queues `work` on `account`'s lane, and starts it if the lane is free.
    pub(super) async fn enqueue(&mut self, account: AccountId, work: LaneWork) {
        let lane = self.accounts.lanes.entry(account).or_default();
        lane.waiting.push_back(work);
        if !lane.busy {
            self.next_in_lane(account).await;
        }
    }

    /// The account's lane is free again: starts what waits next.
    pub(super) async fn lane_done(&mut self, account: AccountId) {
        if let Some(lane) = self.accounts.lanes.get_mut(&account) {
            lane.busy = false;
        }
        self.next_in_lane(account).await;
    }

    /// Starts the next work waiting in a free lane, skipping work that
    /// cannot start.
    async fn next_in_lane(&mut self, account: AccountId) {
        loop {
            let Some(lane) = self.accounts.lanes.get_mut(&account) else {
                return;
            };
            if lane.busy {
                return;
            }
            let Some(work) = lane.waiting.pop_front() else {
                self.accounts.lanes.remove(&account);
                return;
            };
            lane.busy = true;
            let started = match work {
                LaneWork::Check => self.start_check(account).await,
                LaneWork::Resolve(pending) => self.start_resolve(pending).await,
            };
            if started {
                return;
            }
            if let Some(lane) = self.accounts.lanes.get_mut(&account) {
                lane.busy = false;
            }
        }
    }

    /// Calls the provider for `pending` in a task of its own, with the
    /// account's credentials as they are now. Returns whether it started;
    /// if not, the request was answered or is stale.
    async fn start_resolve(&mut self, pending: Box<Pending>) -> bool {
        let account = pending.route.account;
        if pending.request.reply.is_closed() {
            // The download stopped waiting: a new intent was recorded for
            // nothing, and is finished as certainly not sent.
            if let Step::Ask(op) = &pending.step {
                let id = op.id;
                let outcome = OperationOutcome::Failed("not sent: the download stopped".into());
                let _ = self
                    .with_store(move |store| store.finish_operation(id, &outcome, false, now()))
                    .await;
            }
            return false;
        }
        let keys = match self.usable_account(&pending.route).await {
            Ok((keys, _)) => keys,
            Err(failed) => {
                if let Step::Ask(op) = &pending.step {
                    let id = op.id;
                    let why = failed.message.as_ref().map(|m| m.as_str().to_owned());
                    let outcome = OperationOutcome::Failed(why.unwrap_or_default());
                    let _ = self
                        .with_store(move |store| store.finish_operation(id, &outcome, false, now()))
                        .await;
                }
                let _ = pending.request.reply.send(Resolution::Failed(failed));
                return false;
            }
        };
        let loaded = self
            .with_store(move |store| store.account_credential(account))
            .await;
        let opened = loaded.ok().and_then(|(row, stored)| {
            keys.open(&stored.reference, &row.service, &stored.sealed)
                .ok()
                .map(|credential| (row.generation, row.sign_in, credential))
        });
        let Some((generation, sign_in, credential)) = opened else {
            if let Step::Ask(op) = &pending.step {
                let id = op.id;
                let outcome =
                    OperationOutcome::Failed("not sent: the credentials could not be read".into());
                let _ = self
                    .with_store(move |store| store.finish_operation(id, &outcome, false, now()))
                    .await;
            }
            let _ = pending.request.reply.send(Resolution::Failed(failure(
                FailureCode::InvalidCredentials,
                "The account's stored sign-in could not be read. Sign it in again, then retry.",
                None,
            )));
            return false;
        };
        let Some(provider) = self.accounts.provider(&pending.route.service) else {
            return false;
        };
        let client = self.client.clone();
        let cancel = self.supervisor.cancel.clone();
        self.supervisor.spawn(async move {
            let link = pending.request.link.clone();
            let deadline = |what: &str| format!("no answer {what} within {PROVIDER_DEADLINE:?}");
            let look = |credential: &Secret, since: UtcTimestamp| {
                let provider = provider.clone();
                let (credential, link) = (credential.clone(), link.clone());
                async move {
                    tokio::time::timeout(
                        PROVIDER_DEADLINE,
                        provider.find_generated(&credential, &link, since),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        WithCredentials::unchanged(Reconciled::Unknown(
                            "the provider's record did not answer in time".into(),
                        ))
                    })
                }
            };
            // An engine stopping abandons the call; its intent stays, and
            // the next start reconciles it.
            let work = async {
                match &pending.step {
                    Step::Ask(op) => {
                        let asked = tokio::time::timeout(
                            PROVIDER_DEADLINE,
                            provider.unrestrict(&credential, &link),
                        )
                        .await
                        .unwrap_or_else(|_| {
                            WithCredentials::unchanged(Unrestricted::Ambiguous(deadline(
                                "to the request",
                            )))
                        });
                        let mut refreshed = asked.refreshed;
                        let reconciled = if let Unrestricted::Ambiguous(_) = &asked.result {
                            let current = refreshed.as_ref().unwrap_or(&credential);
                            let looked = look(current, since(op)).await;
                            if looked.refreshed.is_some() {
                                refreshed = looked.refreshed;
                            }
                            Some(looked.result)
                        } else {
                            None
                        };
                        (Found::Asked(asked.result, reconciled), refreshed)
                    }
                    Step::Reconcile { operation, .. } => {
                        let looked = look(&credential, since(operation)).await;
                        (Found::Looked(looked.result), looked.refreshed)
                    }
                }
            };
            let (found, refreshed) = tokio::select! {
                () = cancel.cancelled() => return,
                done = work => done,
            };
            let resolved = Resolved {
                pending,
                generation,
                sign_in,
                found,
                refreshed,
            };
            let _ = client.0.send(Internal::Resolved(Box::new(resolved))).await;
        });
        true
    }

    /// A provider task ended: keeps refreshed credentials, records the
    /// operation's outcome, and answers the download if it still wants it.
    pub(super) async fn resolved(&mut self, resolved: Box<Resolved>) {
        let Resolved {
            pending,
            generation,
            sign_in,
            found,
            refreshed,
        } = *resolved;
        let Pending {
            request,
            route,
            step,
        } = *pending;
        let account = route.account;
        if let Some(credential) = refreshed {
            self.keep_refreshed(&route, generation, &credential).await;
        }
        let usable = self.usable_account(&route).await;
        let (job, asking) = (request.job, request.generation);
        let wanted = self
            .with_store(move |store| store.job_state(job))
            .await
            .is_ok_and(|s| {
                s.generation() == asking
                    && s.stage().is_active()
                    && s.stage() != JobStage::Publishing
                    && s.desired() == DesiredState::Running
            });
        // Signed in again since the task read the credentials: its answer
        // is recorded, never used.
        let signed_in_again = usable
            .as_ref()
            .is_ok_and(|&(_, current)| current != sign_in);
        let keep = wanted && usable.is_ok() && !signed_in_again;
        let operation = match &step {
            Step::Ask(op) | Step::Reconcile { operation: op, .. } => op.clone(),
        };
        let repeat = matches!(step, Step::Reconcile { repeat: true, .. });
        let found = self
            .unclaimed(found, &operation, &request.link, account)
            .await;
        let generated = match found {
            Found::Asked(Unrestricted::Generated(link), _)
            | Found::Asked(Unrestricted::Ambiguous(_), Some(Reconciled::Found(link)))
            | Found::Looked(Reconciled::Found(link)) => Ok(checked_link(link)),
            Found::Asked(
                Unrestricted::Refused {
                    code,
                    message,
                    retry_after,
                },
                _,
            ) => Err((code, message, retry_after)),
            Found::Asked(Unrestricted::Ambiguous(why), _) => {
                Err((FailureCode::OperationAmbiguous, why, None))
            }
            Found::Looked(Reconciled::NotFound | Reconciled::Inconclusive(_)) if repeat => {
                // The user confirmed asking again: the old intent is
                // retired and a new one recorded before the request.
                let id = operation.id;
                let retired = self
                    .with_store(move |store| store.retire_operation(id, now()))
                    .await;
                if retired.is_err() {
                    self.lane_done(account).await;
                    let _ = request.reply.send(Resolution::Failed(failure(
                        FailureCode::OperationAmbiguous,
                        UNSAVED,
                        None,
                    )));
                    return;
                }
                if keep && let Some(pending) = self.intend(request, route).await {
                    if !self.start_resolve(Box::new(pending)).await {
                        self.lane_done(account).await;
                    }
                    return;
                }
                self.lane_done(account).await;
                return;
            }
            Found::Looked(Reconciled::NotFound) => {
                Err((FailureCode::OperationAmbiguous, String::new(), None))
            }
            Found::Looked(Reconciled::Inconclusive(why)) => {
                Err((FailureCode::OperationAmbiguous, why, None))
            }
            Found::Looked(Reconciled::Unknown(why)) if repeat => {
                // Still confirmed: a later retry looks again.
                self.lane_done(account).await;
                let _ = request.reply.send(Resolution::Failed(failure(
                    FailureCode::RequestTimeout,
                    &format!(
                        "Wayfetch could not check whether the provider already generated this \
                         link ({why}). Retry later."
                    ),
                    None,
                )));
                return;
            }
            Found::Looked(Reconciled::Unknown(why)) => {
                Err((FailureCode::OperationAmbiguous, why, None))
            }
        };
        let id = operation.id;
        let (outcome, answer) = match generated {
            Ok(Some(link)) => match self
                .accounts
                .vault
                .keys()
                .ok()
                .and_then(|keys| seal_link(&keys, &route.service, &link, sign_in))
            {
                Some(outcome) => (
                    outcome,
                    Resolution::Link(ResolvedLink {
                        operation: id,
                        link,
                        route: route.clone(),
                    }),
                ),
                // Generated, but not storable: recorded as ambiguous, so
                // it is never silently asked for again.
                None => (
                    OperationOutcome::Ambiguous("the generated link could not be stored".into()),
                    Resolution::Failed(failure(FailureCode::OperationAmbiguous, AMBIGUOUS, None)),
                ),
            },
            Ok(None) => (
                OperationOutcome::Failed("the provider generated an unusable link".into()),
                Resolution::Failed(failure(
                    FailureCode::AdapterParseFailure,
                    "The provider generated a link Wayfetch will not use (not HTTPS, or \
                     carrying credentials).",
                    None,
                )),
            ),
            Err((FailureCode::OperationAmbiguous, why, _)) => (
                OperationOutcome::Ambiguous(why),
                Resolution::Failed(failure(FailureCode::OperationAmbiguous, AMBIGUOUS, None)),
            ),
            Err((code, message, retry_after)) => {
                let retry_at = retry_after.and_then(|after| now().checked_add(after));
                (
                    OperationOutcome::Failed(message.clone()),
                    Resolution::Failed(failure(code, &message, retry_at)),
                )
            }
        };
        // An answer from an earlier sign-in, even a refusal, says nothing
        // about the current one.
        let answer = match (&outcome, answer) {
            (OperationOutcome::Ambiguous(_), answer) => answer,
            _ if signed_in_again => Resolution::Failed(failure(
                FailureCode::InvalidCredentials,
                SIGNED_IN_AGAIN,
                None,
            )),
            (_, answer) => answer,
        };
        let finished = self
            .with_store(move |store| store.finish_operation(id, &outcome, keep, now()))
            .await;
        self.lane_done(account).await;
        // A link is released only once its outcome is committed. If any
        // outcome, a refusal included, is not, the intent stays and only a
        // confirmed retry reconciles it, so the job says so.
        let answer = match (finished, answer) {
            (Ok(true), answer) => answer,
            (Ok(false), Resolution::Link(_)) => {
                Resolution::Failed(failure(FailureCode::RequestTimeout, CLOSED, None))
            }
            (Ok(false), answer) => answer,
            (Err(_), _) => {
                Resolution::Failed(failure(FailureCode::OperationAmbiguous, UNSAVED, None))
            }
        };
        let answer = match usable {
            Ok(_) => answer,
            Err(failed) => Resolution::Failed(failed),
        };
        if wanted {
            let _ = request.reply.send(answer);
        }
    }

    /// `found`, unless it takes an entry of the provider's record that
    /// another operation through the account may have made for the same
    /// file since `operation`'s intent: such an entry is no evidence of
    /// this operation's outcome.
    async fn unclaimed(
        &mut self,
        found: Found,
        operation: &Operation,
        link: &Url,
        account: AccountId,
    ) -> Found {
        let taken = matches!(
            found,
            Found::Asked(Unrestricted::Ambiguous(_), Some(Reconciled::Found(_)))
                | Found::Looked(Reconciled::Found(_))
        );
        if !taken {
            return found;
        }
        let (id, since) = (operation.id, since(operation));
        let others = self
            .with_store(move |store| store.recent_operation_links(account, id, since))
            .await;
        let claimed = others.map_or(true, |others| {
            others
                .iter()
                .any(|other| Url::parse(other).map_or(true, |other| may_be_same(&other, link)))
        });
        if !claimed {
            return found;
        }
        let why = Reconciled::Inconclusive(
            "another request through this account may have made the provider's entry for this \
             link"
                .into(),
        );
        match found {
            Found::Asked(asked, _) => Found::Asked(asked, Some(why)),
            Found::Looked(_) => Found::Looked(why),
        }
    }

    /// Stores credentials a provider call refreshed, if the account's
    /// credentials are still the generation the call read.
    async fn keep_refreshed(
        &mut self,
        route: &JobRoute,
        generation: Generation,
        credential: &Secret,
    ) {
        let Ok(keys) = self.accounts.vault.keys() else {
            return;
        };
        let Ok(stored) = super::accounts::seal(&keys, &route.service, credential) else {
            return;
        };
        let account = route.account;
        let _ = self
            .with_store(move |store| store.store_refreshed(account, generation, &stored))
            .await;
    }

    /// After an account was disabled or removed: fails the downloads
    /// resolving or transferring through it, saying why, and stops them.
    pub(super) async fn stop_routed(&mut self, account: AccountId, why: &'static str) {
        let jobs = self
            .with_store(move |store| {
                let jobs = store.active_routed_jobs(account).unwrap_or_default();
                jobs.into_iter()
                    .filter(|&(job, generation)| {
                        store
                            .record_stage(
                                job,
                                JobStage::Failed,
                                generation,
                                Some(failure(FailureCode::InvalidCredentials, why, None)),
                                now(),
                            )
                            .is_ok()
                    })
                    .map(|(job, _)| job)
                    .collect::<Vec<_>>()
            })
            .await;
        for job in jobs {
            if let Some(token) = self.running.get(&job) {
                token.cancel();
            }
        }
    }
}
