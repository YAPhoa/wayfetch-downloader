//! Link checks on the coordinator: finding the links in a collect's text,
//! and running the checks the frontier holds (link crawler stage 1).
//!
//! At most [`MAX_CHECKS`] checks run at once, one per queue (a host, or a
//! video service), each outside the coordinator under [`CHECK_DEADLINE`].
//! The checks waiting are rows in the store, so a restart picks them up
//! again; one running when the engine stopped is sent again. A check's
//! answer is recorded only if it is still its link's check.
//!
//! A link an enabled account would download, as starting it would route
//! it, is checked by asking that account's provider instead of the hoster,
//! which seldom answers anonymous requests for its files. The provider's
//! check needs no sign-in and spends no quota (ADR-08).
//!
//! Which links on a page are collected is decided here, where the video
//! services are known: links a service recognizes and links that look like
//! files. Links to other pages are counted and left alone, so analyzing a
//! page never spiders a site.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wayfetch_domain::id::SourceLinkId;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::command::{InputUrl, MAX_SUBMIT_ITEMS};
use wayfetch_protocol::query::CheckReason;
use wayfetch_protocol::{Command, CommandEnvelope, ErrorCode, ProtocolError};

use super::routing::Capabilities;
use super::{Coordinator, Internal, now};
use crate::accounts::provider::{AccountProvider, Availability, ProviderError, SupportedHosts};
use crate::crawl::extract::{self, Entry};
use crate::crawl::{self as crawler, Checked, Inspector, looks_like_file};
use crate::media::unusable;
use crate::store::{Child, CollectPlan, Collecting, Found, Store, Task};

/// Most checks running at once, across every queue.
pub(super) const MAX_CHECKS: usize = 2;

/// Longest one link check may take: up to three requests, each within its
/// own deadline. A video service's check takes longer.
const CHECK_DEADLINE: Duration = Duration::from_secs(100);

/// Longest a video service's check may take.
const MEDIA_CHECK_DEADLINE: Duration = Duration::from_secs(90);

/// The checks running, and what sends their requests.
#[derive(Default)]
pub(super) struct Checks {
    /// Sends link checks' requests; without it only video links are
    /// checked, and other checks wait.
    inspector: Option<Arc<dyn Inspector>>,
    /// Checks running, with their queues.
    running: BTreeMap<i64, String>,
}

impl Checks {
    pub(super) fn new(inspector: Option<Arc<dyn Inspector>>) -> Self {
        Self {
            inspector,
            running: BTreeMap::new(),
        }
    }
}

/// A provider an enabled account downloads through, with its supported
/// hosts when they were read lately.
#[derive(Clone)]
struct Provider {
    provider: Arc<dyn AccountProvider>,
    hosts: Option<Result<SupportedHosts, ProviderError>>,
}

/// What a check's answer is, to record.
pub(super) enum Answer {
    /// A link check's: what it found, the provider that checked it if not
    /// its own server, and supported hosts read on the way.
    Link(Checked, Option<ProviderId>, Capabilities),
    /// A video service's.
    Media(Result<crate::media::MediaInfo, String>),
}

impl Coordinator {
    /// Finds the links in a collect's text: in HTML, only those that look
    /// like files or that a video service recognizes are collected, and the
    /// rest counted. Text with no links to collect, or more than one
    /// command may hold, is refused.
    pub(super) async fn collect_plan(
        &mut self,
        envelope: &CommandEnvelope,
    ) -> Result<CollectPlan, ProtocolError> {
        let Command::CollectLinks { text, base_url, .. } = &envelope.command else {
            return Ok(CollectPlan::default());
        };
        let (text, base) = (text.clone(), base_url.clone());
        // Parsing half a megabyte of HTML is CPU work.
        let found = tokio::task::spawn_blocking(move || {
            extract::collected(text.as_str(), base.as_ref().map(InputUrl::as_url))
        })
        .await
        .map_err(|_| ProtocolError::new(ErrorCode::Internal, "reading the text failed"))?;
        let mut plan = CollectPlan {
            html: found.html,
            repeated: found.repeated,
            ..CollectPlan::default()
        };
        for entry in found.entries {
            let media = match &entry {
                Entry::Valid(url) => self
                    .media
                    .recognize(url.as_url())
                    .map(|s| s.service().clone()),
                Entry::Invalid { .. } => None,
            };
            if found.html
                && media.is_none()
                && let Entry::Valid(url) = &entry
                && !looks_like_file(url.as_url())
            {
                plan.skipped += 1;
                continue;
            }
            plan.links.push(Collecting { entry, media });
        }
        if plan.links.is_empty() {
            let message = if plan.skipped > 0 {
                format!(
                    "No file or video links found; the {} links in it lead to other web pages. \
                     Paste the page's address instead to have it analyzed.",
                    plan.skipped
                )
            } else {
                "No links found. Links start with http:// or https://.".to_owned()
            };
            return Err(ProtocolError::new(ErrorCode::InvalidRequest, &message));
        }
        if plan.links.len() > MAX_SUBMIT_ITEMS {
            return Err(ProtocolError::new(
                ErrorCode::LimitExceeded,
                &format!(
                    "found {} links; collect at most {MAX_SUBMIT_ITEMS} at a time",
                    plan.links.len()
                ),
            ));
        }
        Ok(plan)
    }

    /// Starts waiting checks, up to the limit.
    pub(super) async fn run_checks(&mut self) {
        let free = MAX_CHECKS.saturating_sub(self.checks.running.len());
        if free == 0 {
            return;
        }
        let busy: Vec<String> = self.checks.running.values().cloned().collect();
        let links = self.checks.inspector.is_some();
        let Ok(tasks) = self
            .with_store(move |store| store.take_checks(free, &busy, links))
            .await
        else {
            return;
        };
        let providers: Arc<[Provider]> =
            if self.checks.inspector.is_some() && tasks.iter().any(|t| t.media.is_none()) {
                self.check_providers().await.into()
            } else {
                Arc::from([])
            };
        for task in tasks {
            if let Some(service) = task.media.clone() {
                self.check_media(task, &service).await;
            } else if let Some(inspector) = self.checks.inspector.clone() {
                self.checks.running.insert(task.id, task.queue.clone());
                let (sender, cancel) = (self.client.0.clone(), self.supervisor.cancel.clone());
                let providers = providers.clone();
                self.supervisor.spawn(async move {
                    let mut hosts = Capabilities::new();
                    let checked = tokio::select! {
                        () = cancel.cancelled() => return,
                        checked = tokio::time::timeout(
                            CHECK_DEADLINE,
                            check_link(inspector.as_ref(), &providers, &task, &mut hosts),
                        ) => checked,
                    };
                    let (checked, via) = checked.unwrap_or_else(|_| {
                        let late = "Checking took too long.".into();
                        (Checked::Unknown(CheckReason::TimedOut, late), None)
                    });
                    let answer = Answer::Link(checked, via, hosts);
                    let _ = sender
                        .send(Internal::Checked(task.id, task.link, task.url, answer))
                        .await;
                });
            }
        }
    }

    /// The providers that enabled accounts download through, in the order
    /// starting a link would try them.
    async fn check_providers(&mut self) -> Vec<Provider> {
        let Ok(mut accounts) = self.with_store(Store::accounts).await else {
            return Vec::new();
        };
        accounts.sort_by_key(|a| a.id);
        let mut providers: Vec<Provider> = Vec::new();
        for account in accounts.iter().filter(|a| a.enabled) {
            let Some(provider) = self.accounts.provider(&account.service) else {
                continue;
            };
            if !provider.support().downloads
                || providers
                    .iter()
                    .any(|p| p.provider.service() == &account.service)
            {
                continue;
            }
            let hosts = self.routing.cached(&account.service);
            providers.push(Provider { provider, hosts });
        }
        providers
    }

    /// Starts a video service's check of a media link, or records at once
    /// why it cannot run.
    async fn check_media(&mut self, task: Task, service: &ProviderId) {
        let Some(service) = self.media.service(service).cloned() else {
            let problem = "Wayfetch has no integration for this video site any more.";
            let _ = self
                .with_store(move |s| s.record_media(task.id, task.link, Err(problem.into()), now()))
                .await;
            return;
        };
        if !service.ready() {
            let problem = unusable(&service.plugins())
                .unwrap_or_else(|| "A plugin this link needs cannot be used.".into());
            let _ = self
                .with_store(move |s| s.record_media(task.id, task.link, Err(problem), now()))
                .await;
            return;
        }
        self.checks.running.insert(task.id, task.queue.clone());
        let (sender, cancel) = (self.client.0.clone(), self.supervisor.cancel.clone());
        self.supervisor.spawn(async move {
            let checked = tokio::select! {
                () = cancel.cancelled() => return,
                checked = tokio::time::timeout(MEDIA_CHECK_DEADLINE, service.probe(&task.url)) => checked,
            };
            let found = match checked {
                Ok(Ok(info)) => Ok(info),
                Ok(Err(e)) => Err(e.message),
                Err(_) => Err("Checking the video took too long. Check it again.".into()),
            };
            let answer = Answer::Media(found);
            let _ = sender
                .send(Internal::Checked(task.id, task.link, task.url, answer))
                .await;
        });
    }

    /// Records a finished check: for a page that was read, with the links
    /// on it worth collecting.
    pub(super) async fn link_checked(
        &mut self,
        task: i64,
        link: SourceLinkId,
        url: Url,
        answer: Answer,
    ) {
        self.checks.running.remove(&task);
        let (checked, via) = match answer {
            Answer::Media(found) => {
                let _ = self
                    .with_store(move |s| {
                        s.record_media(task, link, found, now())
                            .or_else(|_| s.record_media(task, link, Err(UNRECORDED.into()), now()))
                    })
                    .await;
                return;
            }
            Answer::Link(checked, via, hosts) => {
                self.routing.remember(hosts);
                (checked, via)
            }
        };
        let mut found = Found::default();
        if let Checked::Page {
            links: Some(links), ..
        } = &checked
        {
            let mut page = url.clone();
            page.set_fragment(None);
            for child in links {
                // A link to the page itself, or a place on it.
                let mut target = child.clone();
                target.set_fragment(None);
                if target == page {
                    continue;
                }
                let Ok(child) = InputUrl::new(child.clone()) else {
                    continue;
                };
                let media = self
                    .media
                    .recognize(child.as_url())
                    .map(|s| s.service().clone());
                if media.is_none() && !looks_like_file(child.as_url()) {
                    found.skipped += 1;
                    continue;
                }
                found.children.push(Child { url: child, media });
            }
        }
        let _ = self
            .with_store(move |s| {
                s.record_link_check(task, link, &checked, via.as_ref(), &found, now())
                    .or_else(|_| {
                        let unrecorded =
                            Checked::Unknown(CheckReason::Unreadable, UNRECORDED.into());
                        let found = Found::default();
                        s.record_link_check(task, link, &unrecorded, None, &found, now())
                    })
            })
            .await;
    }
}

/// Checks the link of `task`: through the provider an enabled account
/// would download it with, if any, otherwise by asking its own server.
/// Supported hosts read to decide are kept in `hosts`.
async fn check_link(
    inspector: &dyn Inspector,
    providers: &[Provider],
    task: &Task,
    hosts: &mut Capabilities,
) -> (Checked, Option<ProviderId>) {
    let Some(provider) = route(providers, &task.url, hosts).await else {
        let checked = crawler::check(inspector, &task.url, task.reach, task.analyze).await;
        return (checked, None);
    };
    let checked = match provider.availability(&task.url).await {
        Availability::File { name, size } => Checked::File { name, size },
        Availability::Offline(why) => Checked::Offline(CheckReason::NotFound, why),
        Availability::Unknown(reason, why) => Checked::Unknown(reason, why),
    };
    (checked, Some(provider.service().clone()))
}

/// The provider starting `url` would route it through: the first that
/// recognizes it as its own link, else the first whose supported hosts
/// include its host. Hosts not read lately are read, into `read`; hosts
/// that cannot be read leave the link to its own server.
async fn route(
    providers: &[Provider],
    url: &Url,
    read: &mut Capabilities,
) -> Option<Arc<dyn AccountProvider>> {
    if let Some(p) = providers.iter().find(|p| p.provider.requires_account(url)) {
        return Some(p.provider.clone());
    }
    for p in providers {
        let hosts = match &p.hosts {
            Some(hosts) => hosts.clone(),
            None => {
                let hosts =
                    tokio::time::timeout(crawler::REQUEST_DEADLINE, p.provider.supported_hosts())
                        .await
                        .unwrap_or_else(|_| {
                            Err(ProviderError::Unavailable(
                                "supported hosts request timed out".into(),
                            ))
                        });
                read.insert(p.provider.service().clone(), hosts.clone());
                hosts
            }
        };
        if hosts.is_ok_and(|h| h.supports(url)) {
            return Some(p.provider.clone());
        }
    }
    None
}

/// What a link says when what its check found could not be recorded, so
/// the check ends rather than staying "Checking" until a restart.
const UNRECORDED: &str = "Wayfetch could not record what checking found. Check it again.";
