//! Media services on the coordinator: recognizing media links as they are
//! collected, checking them in the background, and the plugins they need
//! (protocol 4.0).
//!
//! At most [`MAX_CHECKS`] checks run at once, each outside the coordinator
//! under [`CHECK_DEADLINE`]; the links waiting are rows in the store, not
//! a queue in memory, so a restart picks them up again. A service whose
//! plugin is missing fails its links' checks at once, saying so; checks
//! that failed are tried again at the next start, when a plugin may have
//! been installed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wayfetch_domain::id::SourceLinkId;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::command::InputUrl;
use wayfetch_protocol::media::PluginSummary;
use wayfetch_protocol::{Command, CommandEnvelope};

use super::{Coordinator, Internal, now};
use crate::media::{Converter, MediaInfo, MediaService};

/// Most media links checked at once.
const MAX_CHECKS: usize = 2;

/// Longest one check may take.
const CHECK_DEADLINE: Duration = Duration::from_secs(90);

/// The media services and converter a host registered.
#[derive(Default)]
pub(super) struct MediaServices {
    services: Vec<Arc<dyn MediaService>>,
    converter: Option<Arc<dyn Converter>>,
    /// Links being checked.
    checking: BTreeSet<SourceLinkId>,
}

impl MediaServices {
    pub(super) fn new(
        services: Vec<Arc<dyn MediaService>>,
        converter: Option<Arc<dyn Converter>>,
    ) -> Self {
        Self {
            services,
            converter,
            checking: BTreeSet::new(),
        }
    }

    /// The service that recognizes `link`, if any.
    pub(super) fn recognize(&self, link: &Url) -> Option<&Arc<dyn MediaService>> {
        self.services.iter().find(|s| s.recognizes(link))
    }

    /// The service named `id`.
    pub(super) fn service(&self, id: &ProviderId) -> Option<&Arc<dyn MediaService>> {
        self.services.iter().find(|s| s.service() == id)
    }

    /// The converter, if one can be used.
    pub(super) fn converter(&self) -> Option<&Arc<dyn Converter>> {
        self.converter.as_ref().filter(|c| c.ready())
    }

    /// Every plugin, each once, with the services that use it.
    pub(super) fn plugins(&self) -> Vec<PluginSummary> {
        let mut plugins: Vec<PluginSummary> = Vec::new();
        let converter = self.converter.iter().map(|c| {
            let mut plugin = c.plugin();
            plugin.services = self.services.iter().map(|s| s.service().clone()).collect();
            plugin
        });
        for plugin in self
            .services
            .iter()
            .flat_map(|s| s.plugins())
            .chain(converter)
        {
            match plugins.iter_mut().find(|p| p.id == plugin.id) {
                Some(known) => {
                    for service in plugin.services {
                        if !known.services.contains(&service) {
                            known.services.push(service);
                        }
                    }
                }
                None => plugins.push(plugin),
            }
        }
        plugins
    }

    /// For a collect: the service recognizing each valid entry, by its
    /// position.
    pub(super) fn collected(&self, envelope: &CommandEnvelope) -> BTreeMap<usize, ProviderId> {
        let Command::CollectLinks { items, .. } = &envelope.command else {
            return BTreeMap::new();
        };
        items
            .as_slice()
            .iter()
            .enumerate()
            .filter_map(|(i, raw)| {
                let url = InputUrl::parse(raw.trim()).ok()?;
                Some((i, self.recognize(url.as_url())?.service().clone()))
            })
            .collect()
    }
}

/// Why a link's check failed while its service's plugin is missing.
fn missing_plugin(service: &dyn MediaService) -> String {
    let names: Vec<_> = service
        .plugins()
        .into_iter()
        .filter(|p| p.state != wayfetch_protocol::media::PluginState::Ready)
        .map(|p| p.name)
        .collect();
    format!(
        "Video links need the {} plugin, which is not available. See Settings > Plugins.",
        names.join(" and ")
    )
}

impl Coordinator {
    /// Starts checks of waiting media links, up to the limit.
    pub(super) async fn check_media(&mut self) {
        let free = MAX_CHECKS.saturating_sub(self.media.checking.len());
        if free == 0 || self.media.services.is_empty() {
            return;
        }
        let busy: Vec<_> = self.media.checking.iter().copied().collect();
        let Ok(pending) = self
            .with_store(move |store| store.pending_media(free, &busy))
            .await
        else {
            return;
        };
        for (link, service, url) in pending {
            let Some(service) = self.media.service(&service).cloned() else {
                let problem = "Wayfetch has no integration for this video site any more.";
                let _ = self
                    .with_store(move |s| s.record_media(link, Err(problem.into()), now()))
                    .await;
                continue;
            };
            if !service.ready() {
                let problem = missing_plugin(service.as_ref());
                let _ = self
                    .with_store(move |s| s.record_media(link, Err(problem), now()))
                    .await;
                continue;
            }
            self.media.checking.insert(link);
            let (sender, cancel) = (self.client.0.clone(), self.supervisor.cancel.clone());
            self.supervisor.spawn(async move {
                let checked = tokio::select! {
                    () = cancel.cancelled() => return,
                    checked = tokio::time::timeout(CHECK_DEADLINE, service.probe(&url)) => checked,
                };
                let found = match checked {
                    Ok(Ok(info)) => Ok(info),
                    Ok(Err(e)) => Err(e.message),
                    Err(_) => {
                        Err("Checking the video took too long. Start it to try again.".into())
                    }
                };
                let _ = sender.send(Internal::MediaChecked(link, found)).await;
            });
        }
    }

    /// Records a finished check; `changed` starts the next.
    pub(super) async fn media_checked(
        &mut self,
        link: SourceLinkId,
        found: Result<MediaInfo, String>,
    ) {
        self.media.checking.remove(&link);
        let _ = self
            .with_store(move |s| s.record_media(link, found, now()))
            .await;
    }
}
