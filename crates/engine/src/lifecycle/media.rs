//! Media services on the coordinator: recognizing media links, and the
//! plugins they need. Their links are checked with every
//! other link's, in the crawl module; a service whose plugin is missing
//! fails its links' checks at once, saying so, and checks that failed are
//! tried again at the next start, when a plugin may have been installed.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::media::PluginChanged;
use wayfetch_protocol::media::PluginSummary;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, IdempotencyKey, ProtocolError,
};

use super::{Coordinator, now};
use crate::media::{
    Converter, MediaService, PluginControl, PluginError, PluginOutcome, PluginWork,
};

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
    /// The keys of the plugin changes running, with their commands.
    pub(super) keys: HashMap<IdempotencyKey, Command>,
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
            keys: HashMap::new(),
        }
    }

    /// Why `envelope` cannot run while a plugin change holds its key:
    /// a retry waits for the change, and another command conflicts.
    pub(super) fn held(&self, envelope: &CommandEnvelope) -> Option<ProtocolError> {
        let running = self.keys.get(&envelope.idempotency_key)?;
        Some(if *running == envelope.command {
            ProtocolError::new(
                ErrorCode::Busy,
                "the plugin is still being changed; try again shortly",
            )
        } else {
            ProtocolError::new(
                ErrorCode::IdempotencyConflict,
                "idempotency key was used for a different command",
            )
        })
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
}

impl Coordinator {
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
        // Held until the change is recorded, so the key cannot start
        // anything else meanwhile.
        self.media
            .keys
            .insert(envelope.idempotency_key, envelope.command.clone());
        let sender = self.client.0.clone();
        let stop = self.supervisor.cancel.child_token();
        let work = PluginWork::new(self.supervisor.guard(), stop.clone());
        // The change is never abandoned mid-step: running out of time or
        // shutting down asks it to stop, and it ends at its next step.
        self.supervisor.spawn(async move {
            let change = control.change(&plugin, change, work);
            tokio::pin!(change);
            let result = tokio::select! {
                result = &mut change => result,
                () = tokio::time::sleep(CHANGE_DEADLINE) => {
                    stop.cancel();
                    change.await
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
        self.media.keys.remove(&envelope.idempotency_key);
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
        let (recorded, saved) = self
            .with_store(move |store| {
                store.set_media_converter(converter);
                let recorded = store.record_plugin_change(
                    &envelope,
                    instance,
                    &result,
                    outcome.changed,
                    now(),
                );
                (recorded, store.settings().ok())
            })
            .await;
        // Settings answers carry the revision the change moved to.
        if let Some(saved) = saved {
            self.destinations
                .set_settings(saved.revision, saved.default_root);
        }
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
