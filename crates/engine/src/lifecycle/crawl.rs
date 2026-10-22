//! Link checks on the coordinator: finding links in collected text and
//! running the queued checks.
//!
//! Checks are stored rows, so a restart resumes them. At most [`MAX_CHECKS`]
//! run at once, one per host or video service, each with a deadline. A link
//! an enabled account would download is also checked with that provider.
//! Analyzing a page collects the files and videos it links to, never other
//! pages.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;

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

/// Longest one link check may take: up to three requests to the link's
/// server, each within its own deadline, alongside a provider's check,
/// after reading its supported hosts if needed.
const CHECK_DEADLINE: Duration = Duration::from_secs(130);

/// How long a provider that asked to slow down is not asked to check
/// links. Providers may count refused requests toward their limit.
const PROVIDER_COOLDOWN: Duration = Duration::from_secs(60);

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
    /// Providers that asked to slow down, until when.
    cooling: HashMap<ProviderId, Instant>,
}

impl Checks {
    pub(super) fn new(inspector: Option<Arc<dyn Inspector>>) -> Self {
        Self {
            inspector,
            running: BTreeMap::new(),
            cooling: HashMap::new(),
        }
    }
}

/// A provider an enabled account downloads through, with its supported
/// hosts when they were read lately, and whether it asked to slow down.
#[derive(Clone)]
struct Provider {
    provider: Arc<dyn AccountProvider>,
    hosts: Option<Result<SupportedHosts, ProviderError>>,
    cooling: bool,
}

/// What a link check found.
pub(super) struct LinkAnswer {
    /// What the link shows: the provider's answer when one checked it,
    /// else its own server's.
    checked: Checked,
    /// Its own server's answer, when a provider's is shown instead.
    origin: Option<Checked>,
    /// The provider that checked it.
    via: Option<ProviderId>,
    /// Supported hosts read on the way.
    hosts: Capabilities,
    /// Whether the provider asked to slow down.
    limited: bool,
}

/// What a check's answer is, to record.
pub(super) enum Answer {
    /// A link check's.
    Link(Box<LinkAnswer>),
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
                let (providers, runtime) = (providers.clone(), self.supervisor.runtime.clone());
                self.supervisor.spawn(async move {
                    let checked = tokio::select! {
                        () = cancel.cancelled() => return,
                        checked = tokio::time::timeout(
                            CHECK_DEADLINE,
                            check_link(inspector.as_ref(), &providers, &task, &runtime),
                        ) => checked,
                    };
                    let answer = checked.unwrap_or_else(|_| LinkAnswer {
                        checked: Checked::Unknown(
                            CheckReason::TimedOut,
                            "Checking took too long.".into(),
                        ),
                        origin: None,
                        via: None,
                        hosts: Capabilities::new(),
                        limited: false,
                    });
                    let answer = Answer::Link(Box::new(answer));
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
        let now = Instant::now();
        self.checks.cooling.retain(|_, until| *until > now);
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
            let cooling = self.checks.cooling.contains_key(&account.service);
            providers.push(Provider {
                provider,
                hosts,
                cooling,
            });
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
        let answer = match answer {
            Answer::Media(found) => {
                let _ = self
                    .with_store(move |s| {
                        s.record_media(task, link, found, now())
                            .or_else(|_| s.record_media(task, link, Err(UNRECORDED.into()), now()))
                    })
                    .await;
                return;
            }
            Answer::Link(answer) => *answer,
        };
        let LinkAnswer {
            checked,
            origin,
            via,
            hosts,
            limited,
        } = answer;
        self.routing.remember(hosts);
        if limited && let Some(via) = &via {
            let until = Instant::now() + PROVIDER_COOLDOWN;
            self.checks.cooling.insert(via.clone(), until);
        }
        let mut found = Found::default();
        if let Checked::Page {
            links: Some(links), ..
        } = origin.as_ref().unwrap_or(&checked)
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
                let shown = (&checked, via.as_ref());
                s.record_link_check(task, link, shown, origin.as_ref(), &found, now())
                    .or_else(|_| {
                        let unrecorded =
                            Checked::Unknown(CheckReason::Unreadable, UNRECORDED.into());
                        let found = Found::default();
                        s.record_link_check(task, link, (&unrecorded, None), None, &found, now())
                    })
            })
            .await;
    }
}

/// Checks the link of `task` with its own server and, if an enabled
/// account would download it, with that account's provider too.
async fn check_link(
    inspector: &dyn Inspector,
    providers: &[Provider],
    task: &Task,
    runtime: &Handle,
) -> LinkAnswer {
    let mut hosts = Capabilities::new();
    let origin = crawler::check(inspector, &task.url, task.reach, task.analyze);
    let Some(provider) = route(providers, &task.url, runtime, &mut hosts).await else {
        return LinkAnswer {
            checked: origin.await,
            origin: None,
            via: None,
            hosts,
            limited: false,
        };
    };
    let available = async {
        if provider.cooling {
            return Availability::Unknown(
                CheckReason::RateLimited,
                "The service asked Wayfetch to slow down; check again in a minute.".into(),
            );
        }
        let (service, url) = (provider.provider.clone(), task.url.clone());
        isolated(runtime, async move {
            tokio::time::timeout(crawler::REQUEST_DEADLINE, service.availability(&url)).await
        })
        .await
        .unwrap_or_else(|| {
            Availability::Unknown(
                CheckReason::Unreadable,
                "The service's check of this link failed.".into(),
            )
        })
    };
    let (origin, available) = tokio::join!(origin, available);
    let limited = !provider.cooling
        && matches!(
            available,
            Availability::Unknown(CheckReason::RateLimited, _)
        );
    let checked = match available {
        Availability::File { name, size } => Checked::File { name, size },
        Availability::Offline(why) => Checked::Offline(CheckReason::NotFound, why),
        Availability::Unknown(reason, why) => Checked::Unknown(reason, why),
    };
    LinkAnswer {
        checked,
        origin: Some(origin),
        via: Some(provider.provider.service().clone()),
        hosts,
        limited,
    }
}

/// Runs a provider call as a task of its own on `runtime`: `None` if it
/// panicked or timed out, which fails only this check. The task is aborted
/// if the check is dropped, as at shutdown.
async fn isolated<T: Send + 'static>(
    runtime: &Handle,
    call: impl Future<Output = Result<T, tokio::time::error::Elapsed>> + Send + 'static,
) -> Option<T> {
    let mut work = tokio::task::JoinSet::new();
    work.spawn_on(call, runtime);
    work.join_next().await?.ok()?.ok()
}

/// The provider starting `url` would route it through: the first that
/// recognizes it as its own link, else the first whose supported hosts
/// include its host. Hosts not read lately are read, into `read`; hosts
/// that cannot be read leave the link to its own server.
async fn route<'a>(
    providers: &'a [Provider],
    url: &Url,
    runtime: &Handle,
    read: &mut Capabilities,
) -> Option<&'a Provider> {
    if let Some(p) = providers.iter().find(|p| p.provider.requires_account(url)) {
        return Some(p);
    }
    for p in providers {
        let hosts = match &p.hosts {
            Some(hosts) => hosts.clone(),
            None => {
                let provider = p.provider.clone();
                let hosts = isolated(runtime, async move {
                    tokio::time::timeout(crawler::REQUEST_DEADLINE, provider.supported_hosts())
                        .await
                })
                .await
                .unwrap_or_else(|| {
                    Err(ProviderError::Unavailable(
                        "supported hosts could not be read".into(),
                    ))
                });
                read.insert(p.provider.service().clone(), hosts.clone());
                hosts
            }
        };
        if hosts.is_ok_and(|h| h.supports(url)) {
            return Some(p);
        }
    }
    None
}

/// What a link says when what its check found could not be recorded, so
/// the check ends rather than staying "Checking" until a restart.
const UNRECORDED: &str = "Wayfetch could not record what checking found. Check it again.";
