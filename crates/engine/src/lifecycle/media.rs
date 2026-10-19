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
use wayfetch_protocol::media::PluginChanged;
use wayfetch_protocol::media::PluginSummary;
use wayfetch_protocol::{Command, CommandEnvelope, CommandResult, ErrorCode, ProtocolError};

use super::{Coordinator, Internal, now};
use crate::media::{
    Converter, MediaInfo, MediaService, PluginControl, PluginError, PluginOutcome, unusable,
};

/// Most media links checked at once.
const MAX_CHECKS: usize = 2;

/// Longest one check may take.
const CHECK_DEADLINE: Duration = Duration::from_secs(90);

/// Longest one plugin change may take: an update downloads a release of
/// up to about 40 MB and runs it once.
const CHANGE_DEADLINE: Duration = Duration::from_secs(15 * 60);

type Reply = tokio::sync::oneshot::Sender<Result<CommandResult, ProtocolError>>;

/// The media services and converter a host registered.
#[derive(Default)]
pub(super) struct MediaServices {
    services: Vec<Arc<dyn MediaService>>,
    converter: Option<Arc<dyn Converter>>,
    /// Changes plugins, if the host can.
    pub(super) control: Option<Arc<dyn PluginControl>>,
    /// Plugins being changed.
    pub(super) changing: BTreeSet<wayfetch_domain::name::PluginId>,
    /// Links being checked.
    checking: BTreeSet<SourceLinkId>,
}

impl MediaServices {
    pub(super) fn new(
        services: Vec<Arc<dyn MediaService>>,
        converter: Option<Arc<dyn Converter>>,
        control: Option<Arc<dyn PluginControl>>,
    ) -> Self {
        Self {
            services,
            converter,
            control,
            changing: BTreeSet::new(),
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
                let problem = unusable(&service.plugins())
                    .unwrap_or_else(|| "A plugin this link needs cannot be used.".into());
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

    /// Runs [`Command::ChangePlugin`]: replays a recorded result, or has
    /// the host make the change in a task of its own, one change of a
    /// plugin at a time.
    pub(super) async fn change_plugin(&mut self, envelope: Box<CommandEnvelope>, reply: Reply) {
        let Command::ChangePlugin { plugin, change } = &envelope.command else {
            return;
        };
        let (plugin, change) = (plugin.clone(), *change);
        let instance = self.identity.instance;
        let replay = envelope.clone();
        match self
            .with_store(move |store| store.replayed_command(&replay, instance))
            .await
        {
            Ok(None) => {}
            Ok(Some(result)) => {
                let _ = reply.send(Ok(result));
                return;
            }
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        }
        let Some(control) = self.media.control.clone() else {
            let _ = reply.send(Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "this Wayfetch cannot change its plugins",
            )));
            return;
        };
        if !self.media.changing.insert(plugin.clone()) {
            let _ = reply.send(Err(plugin_error(PluginError::Busy(format!(
                "{plugin} is already being changed"
            )))));
            return;
        }
        let (sender, cancel) = (self.client.0.clone(), self.supervisor.cancel.clone());
        self.supervisor.spawn(async move {
            let result = tokio::select! {
                () = cancel.cancelled() => return,
                result = tokio::time::timeout(CHANGE_DEADLINE, control.change(&plugin, change)) => {
                    result.unwrap_or_else(|_| {
                        Err(PluginError::Unreachable("the change took too long".into()))
                    })
                }
            };
            let _ = sender
                .send(super::Internal::PluginChanged {
                    envelope,
                    result,
                    reply,
                })
                .await;
        });
    }

    /// Records a plugin change that ended, and makes media links and
    /// downloads use the plugins as they now are.
    pub(super) async fn plugin_changed(
        &mut self,
        envelope: Box<CommandEnvelope>,
        result: Result<PluginOutcome, PluginError>,
        reply: Reply,
    ) {
        let Command::ChangePlugin { plugin, .. } = &envelope.command else {
            return;
        };
        self.media.changing.remove(plugin);
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                let _ = reply.send(Err(plugin_error(e)));
                return;
            }
        };
        let result = CommandResult::PluginChanged(PluginChanged {
            plugin: plugin.clone(),
            changed: outcome.changed,
            version: outcome.version,
        });
        let (instance, converter) = (self.identity.instance, self.media.converter().is_some());
        let recorded = self
            .with_store(move |store| {
                store.set_media_converter(converter);
                store.record_plugin_change(&envelope, instance, &result, outcome.changed, now())
            })
            .await;
        let _ = reply.send(recorded);
        self.changed().await;
    }
}

/// What a client is told about a refused or failed plugin change.
fn plugin_error(err: PluginError) -> ProtocolError {
    let code = match &err {
        PluginError::Unknown(_) => ErrorCode::NotFound,
        PluginError::NotApplicable(_) | PluginError::Rejected(_) => ErrorCode::NotApplicable,
        PluginError::Busy(_) | PluginError::Unreachable(_) => ErrorCode::Busy,
        PluginError::Storage(_) => ErrorCode::Internal,
    };
    ProtocolError::new(code, &err.to_string())
}
