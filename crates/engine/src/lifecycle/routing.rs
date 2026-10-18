//! Engine-owned automatic routing. Public, non-consumptive capabilities are
//! cached briefly and fetched single-flight outside the coordinator; each
//! pending start is bounded and rechecks current account state afterwards.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use wayfetch_domain::id::SourceLinkId;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::account::AccountStatus;
use wayfetch_protocol::query::JobRoute;
use wayfetch_protocol::{Command, CommandEnvelope, CommandResult, ErrorCode, ProtocolError};

use super::{Coordinator, Internal, now};
use crate::accounts::provider::{ProviderError, SupportedHosts};
use crate::store::Store;

type Reply = oneshot::Sender<Result<CommandResult, ProtocolError>>;
pub(super) type Routes = BTreeMap<SourceLinkId, Result<Option<JobRoute>, ProtocolError>>;
pub(super) type Capabilities = BTreeMap<ProviderId, Result<SupportedHosts, ProviderError>>;
const CACHE_FOR: Duration = Duration::from_secs(5 * 60);
const FAILURE_FOR: Duration = Duration::from_secs(15);
const MAX_PENDING: usize = 4;

#[derive(Default)]
pub(super) struct RoutingService {
    cache: BTreeMap<ProviderId, (Instant, Result<SupportedHosts, ProviderError>)>,
    pending: Vec<(Box<CommandEnvelope>, Reply)>,
    fetching: bool,
}

impl Coordinator {
    pub(super) async fn start_routing(&mut self, envelope: Box<CommandEnvelope>, reply: Reply) {
        if envelope.target != self.identity.instance {
            // Let the durable command validator report the target error.
            self.execute_command(envelope, reply, Routes::new()).await;
            return;
        }
        match self.replays(&envelope).await {
            Ok(true) => {
                self.execute_command(envelope, reply, Routes::new()).await;
                return;
            }
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            Ok(false) => {}
        }
        let accounts = match self.with_store(Store::accounts).await {
            Ok(accounts) => accounts,
            Err(error) => {
                let _ = reply.send(Err(error.into()));
                return;
            }
        };
        let services: BTreeSet<_> = accounts
            .iter()
            .filter(|a| a.enabled)
            .filter(|a| {
                self.accounts
                    .provider(&a.service)
                    .is_some_and(|p| p.support().downloads)
            })
            .map(|a| a.service.clone())
            .collect();
        self.routing.cache.retain(|service, (at, result)| {
            services.contains(service)
                && at.elapsed()
                    < if result.is_ok() {
                        CACHE_FOR
                    } else {
                        FAILURE_FOR
                    }
        });
        let missing: Vec<_> = services
            .iter()
            .filter(|s| !self.routing.cache.contains_key(*s))
            .filter_map(|s| self.accounts.provider(s))
            .collect();
        if !missing.is_empty() {
            if self.routing.pending.len() >= MAX_PENDING {
                let _ = reply.send(Err(ProtocolError::new(
                    ErrorCode::Busy,
                    "too many automatic starts are waiting for supported hosts; try again shortly",
                )));
                return;
            }
            self.routing.pending.push((envelope, reply));
            if !self.routing.fetching {
                self.routing.fetching = true;
                let sender = self.client.0.clone();
                let cancel = self.supervisor.cancel.clone();
                let runtime = self.supervisor.runtime.clone();
                self.supervisor.spawn(async move {
                    // A provider panic is unknown support. JoinSet owns and
                    // reaps the child even during shutdown, so neither pending
                    // starts nor provider work are detached.
                    let failed: Capabilities = missing
                        .iter()
                        .map(|p| {
                            (
                                p.service().clone(),
                                Err(ProviderError::Unavailable(
                                    "supported hosts task failed".into(),
                                )),
                            )
                        })
                        .collect();
                    let mut work = tokio::task::JoinSet::new();
                    work.spawn_on(
                        async move {
                            let mut results = BTreeMap::new();
                            let deadline =
                                tokio::time::Instant::now() + super::accounts::PROVIDER_DEADLINE;
                            for provider in missing {
                                let result =
                                    tokio::time::timeout_at(deadline, provider.supported_hosts())
                                        .await
                                        .unwrap_or_else(|_| {
                                            Err(ProviderError::Unavailable(
                                                "supported hosts request timed out".into(),
                                            ))
                                        });
                                results.insert(provider.service().clone(), result);
                            }
                            results
                        },
                        &runtime,
                    );
                    tokio::select! {
                        () = cancel.cancelled() => { work.shutdown().await; }
                        result = work.join_next() => {
                            let results = result.and_then(Result::ok).unwrap_or(failed);
                            let _ = sender.send(Internal::HostsReady(results)).await;
                        }
                    }
                });
            }
            return;
        }
        let links = match &envelope.command {
            Command::StartLinks { links, .. } => {
                links.as_slice().iter().map(|l| l.link).collect::<Vec<_>>()
            }
            _ => unreachable!(),
        };
        let urls = match self.with_store(move |store| store.start_urls(&links)).await {
            Ok(urls) => urls,
            Err(error) => {
                let _ = reply.send(Err(error.into()));
                return;
            }
        };
        // Accounts are sorted by durable ID, independent of provider/catalogue order.
        let mut accounts = accounts;
        accounts.sort_by_key(|a| a.id);
        // Open each eligible account's credential only once per batch.
        let mut usable = BTreeSet::new();
        for account in accounts
            .iter()
            .filter(|a| a.enabled && services.contains(&a.service))
        {
            if account.status != AccountStatus::Authenticated
                || account.expires_at.is_some_and(|at| at <= now())
            {
                continue;
            }
            let id = account.id;
            let stored = self
                .with_store(move |store| store.account_credential(id))
                .await;
            if stored.ok().is_some_and(|(row, stored)| {
                self.accounts.vault.keys().ok().is_some_and(|keys| {
                    keys.open(&stored.reference, &row.service, &stored.sealed)
                        .is_ok()
                })
            }) {
                usable.insert(id);
            }
        }
        let mut routes = Routes::new();
        for (link, url) in urls {
            let mut matching = Vec::new();
            let mut unknown = false;
            for account in accounts
                .iter()
                .filter(|a| a.enabled && services.contains(&a.service))
            {
                match &self.routing.cache[&account.service].1 {
                    Ok(hosts) if hosts.supports(&url) => matching.push(account),
                    Ok(_) => {}
                    Err(_) => unknown = true,
                }
            }
            let mut route = None;
            for account in &matching {
                if usable.contains(&account.id) {
                    route = Some(JobRoute {
                        service: account.service.clone(),
                        account: account.id,
                    });
                    break;
                }
            }
            let decision = if route.is_some() {
                Ok(route)
            } else if !matching.is_empty() {
                Err(ProtocolError::new(
                    ErrorCode::NotApplicable,
                    "A provider supports this host, but its enabled accounts cannot be used. Check account status, expiry, and credential storage in Accounts, or choose Download via Direct.",
                ))
            } else if unknown {
                Err(ProtocolError::new(
                    ErrorCode::Busy,
                    "Supported hosts could not be read. Try again shortly, or choose Download via Direct.",
                ))
            } else {
                Ok(None)
            };
            routes.insert(link, decision);
        }
        self.execute_command(envelope, reply, routes).await;
    }

    pub(super) async fn hosts_ready(&mut self, capabilities: Capabilities) {
        self.routing.fetching = false;
        for (service, result) in capabilities {
            self.routing.cache.insert(service, (Instant::now(), result));
        }
        let pending = std::mem::take(&mut self.routing.pending);
        for (envelope, reply) in pending {
            self.start_routing(envelope, reply).await;
        }
    }
}
