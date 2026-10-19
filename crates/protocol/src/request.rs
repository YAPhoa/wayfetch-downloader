//! Request bodies that clients send for commands, and their translation to
//! [`CommandEnvelope`]s.
//!
//! The browser posts them over HTTP and the desktop shell passes them over
//! IPC, so both transports validate inputs, report a bad item by its
//! position, and build the same envelope (architecture brief section 4.7,
//! test T62).

use serde::Deserialize;
use wayfetch_domain::id::{AccountId, JobId, PackageId, RootId};
use wayfetch_domain::name::{PluginId, ProviderId};
use wayfetch_domain::version::Revision;

use crate::account::{AccountAlias, AccountRef};
use crate::command::{
    CollectItems, Command, CommandEnvelope, IdempotencyKey, InputUrl, LinkRef, LinkRefs,
    PreferenceChange, SpeedLimitChange, StartRouting, SubmitItems,
};
use crate::error::{ErrorCode, ProtocolError};
use crate::instance::InstanceId;
use crate::media::{MediaChoice, PluginChange};

/// Submits a batch of URLs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SubmitRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// Raw, so each URL is validated with its own error code and position.
    pub items: Vec<String>,
    /// Package to add the jobs to.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub package: Option<PackageId>,
}

impl SubmitRequest {
    /// The command envelope, or the first invalid item's error, naming its
    /// position.
    pub fn into_envelope(self) -> Result<CommandEnvelope, ProtocolError> {
        let items = self
            .items
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                InputUrl::parse(raw).map_err(|e| {
                    ProtocolError::new(e.code, &format!("item {} rejected: {}", i + 1, e.message))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command: Command::Submit {
                items: SubmitItems::try_from(items)?,
                package: self.package,
            },
        })
    }
}

/// A job command's action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Action {
    /// [`Command::Pause`].
    Pause,
    /// [`Command::Resume`].
    Resume,
    /// [`Command::Cancel`].
    Cancel,
    /// [`Command::Rename`], with the request's `name`. Added in protocol
    /// 1.0.
    Rename,
    /// [`Command::Remove`]. Added in protocol 1.0.
    Remove,
    /// [`Command::DeleteFiles`]. Added in protocol 2.4.
    DeleteFiles,
    /// [`Command::Retry`]. Added in protocol 1.2.
    Retry,
}

/// Pauses, resumes, cancels, retries, renames, or removes a job at the
/// revision the client saw.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobCommandRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// What to do.
    pub action: Action,
    /// The job revision the client last saw.
    pub expected_revision: Revision,
    /// The new file name, for [`Action::Rename`] only.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub name: Option<String>,
    /// For [`Action::Retry`] only: repeat a provider operation whose
    /// outcome is unknown, accepting possible duplicate provider work
    /// ([`Command::Retry`]). Added in protocol 2.2.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub repeat_provider_operation: Option<bool>,
}

impl JobCommandRequest {
    /// The command envelope for `job`. A name is required for a rename and
    /// refused with any other action; only a retry may repeat a provider
    /// operation.
    pub fn into_envelope(self, job: JobId) -> Result<CommandEnvelope, ProtocolError> {
        let expected_revision = self.expected_revision;
        let repeat_provider_operation = self.repeat_provider_operation.unwrap_or(false);
        if repeat_provider_operation && self.action != Action::Retry {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "only a retry repeats a provider operation",
            ));
        }
        let command = match (self.action, self.name) {
            (Action::Rename, Some(name)) => Command::Rename {
                job,
                expected_revision,
                name,
            },
            (Action::Rename, None) => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "a rename needs a name",
                ));
            }
            (_, Some(_)) => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "only a rename takes a name",
                ));
            }
            (Action::Pause, None) => Command::Pause {
                job,
                expected_revision,
            },
            (Action::Resume, None) => Command::Resume {
                job,
                expected_revision,
            },
            (Action::Cancel, None) => Command::Cancel {
                job,
                expected_revision,
            },
            (Action::DeleteFiles, None) => Command::DeleteFiles {
                job,
                expected_revision,
            },
            (Action::Remove, None) => Command::Remove {
                job,
                expected_revision,
            },
            (Action::Retry, None) => Command::Retry {
                job,
                expected_revision,
                repeat_provider_operation,
            },
        };
        Ok(CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command,
        })
    }
}

/// Changes settings at the revision the client saw: the default
/// destination, engine preferences, or both, all or none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UpdateSettingsRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// The settings revision the client last saw.
    pub expected_revision: Revision,
    /// Where jobs submitted from now on are saved. Optional since protocol
    /// 2.0.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub default_root: Option<RootId>,
    /// Most downloads transferring at once. Added in protocol 2.0.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub max_active_downloads: Option<PreferenceChange>,
    /// The total download speed limit. Added in protocol 2.3.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub download_speed_limit: Option<SpeedLimitChange>,
}

impl UpdateSettingsRequest {
    /// The command envelope.
    pub fn into_envelope(self) -> CommandEnvelope {
        CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command: Command::UpdateSettings {
                expected_revision: self.expected_revision,
                default_root: self.default_root,
                max_active_downloads: self.max_active_downloads,
                download_speed_limit: self.download_speed_limit,
            },
        }
    }
}

/// Stops offering a folder as a destination.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RemoveRootRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// The settings revision the client last saw.
    pub expected_revision: Revision,
    /// The folder.
    pub root: RootId,
}

impl RemoveRootRequest {
    /// The command envelope.
    pub fn into_envelope(self) -> CommandEnvelope {
        CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command: Command::RemoveRoot {
                expected_revision: self.expected_revision,
                root: self.root,
            },
        }
    }
}

/// Changes a plugin. Added in protocol 4.1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ChangePluginRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// The plugin.
    pub plugin: PluginId,
    /// The change.
    pub change: PluginChange,
}

impl ChangePluginRequest {
    /// The command envelope.
    pub fn into_envelope(self) -> CommandEnvelope {
        CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command: Command::ChangePlugin {
                plugin: self.plugin,
                change: self.change,
            },
        }
    }
}

/// Collects links for review without starting them. Added in protocol
/// 1.1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CollectRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// The entries, one link each. An entry that is not a valid link is
    /// collected as invalid rather than refusing the others.
    pub items: Vec<String>,
    /// Where the links will be saved; the default destination if absent.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub destination: Option<RootId>,
}

impl CollectRequest {
    /// The command envelope.
    pub fn into_envelope(self) -> Result<CommandEnvelope, ProtocolError> {
        Ok(CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command: Command::CollectLinks {
                items: CollectItems::try_from(self.items)?,
                destination: self.destination,
            },
        })
    }
}

/// What to do with collected links. Added in protocol 1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum LinksAction {
    /// [`Command::StartLinks`].
    Start,
    /// [`Command::RemoveLinks`].
    Remove,
    /// [`Command::MoveLinks`], with the request's `destination`.
    Move,
    /// [`Command::ChooseLinkMedia`], with the request's `media`. Added in
    /// protocol 4.0.
    ChooseMedia,
}

/// Starts, removes, or moves collected links at the revisions the client
/// saw. Added in protocol 1.1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LinksCommandRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// What to do.
    pub action: LinksAction,
    /// The links, each at the revision the client saw.
    pub links: Vec<LinkRef>,
    /// The new destination, for [`LinksAction::Move`] only.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub destination: Option<RootId>,
    /// For [`LinksAction::Start`] only: the provider account to download
    /// through; automatic selection if absent unless routing requests
    /// Direct. Account override added in protocol 2.2.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub account: Option<AccountId>,
    /// Explicit route intent for a start. Absent uses
    /// automatic selection; cannot be combined with an account. Added in 3.0.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub routing: Option<StartRouting>,
    /// What to download media links as, for [`LinksAction::ChooseMedia`]
    /// only. Added in protocol 4.0.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub media: Option<MediaChoice>,
}

impl LinksCommandRequest {
    /// The command envelope. A destination is required for a move and
    /// refused with any other action.
    pub fn into_envelope(self) -> Result<CommandEnvelope, ProtocolError> {
        let links = LinkRefs::try_from(self.links)?;
        if self.routing.is_some() && (self.action != LinksAction::Start || self.account.is_some()) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "routing is only for a start without an account",
            ));
        }
        if self.account.is_some() && self.action != LinksAction::Start {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "only a start takes an account",
            ));
        }
        if self.media.is_some() != (self.action == LinksAction::ChooseMedia) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "a media choice is needed for, and only taken by, choosing media",
            ));
        }
        let command = match (self.action, self.destination) {
            (LinksAction::Move, Some(destination)) => Command::MoveLinks { links, destination },
            (LinksAction::Move, None) => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "a move needs a destination",
                ));
            }
            (_, Some(_)) => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "only a move takes a destination",
                ));
            }
            (LinksAction::Start, None) => Command::StartLinks {
                links,
                account: self.account,
                routing: self.routing,
            },
            (LinksAction::Remove, None) => Command::RemoveLinks { links },
            (LinksAction::ChooseMedia, None) => Command::ChooseLinkMedia {
                links,
                choice: self.media.unwrap_or_default(),
            },
        };
        Ok(CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command,
        })
    }
}

/// Permits a folder on the engine's host as a destination, by its path.
/// The path is checked and the folder initialized by the engine; choosing
/// it as the default is a separate settings change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AddRootRequest {
    /// The folder, as an absolute path.
    pub path: String,
}

/// Signs in to a provider: adds an account, or replaces an account's
/// credentials. Carries no credentials; the person approves Wayfetch on
/// the provider's site. Added in protocol 1.3.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AuthorizeAccountRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// The service.
    pub service: ProviderId,
    /// The account to sign in again, at the revision the client saw;
    /// absent to add one.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub account: Option<AccountRef>,
    /// A name for a new account; blank for none.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub alias: Option<String>,
}

impl AuthorizeAccountRequest {
    /// The command envelope. An alias names only a new account.
    pub fn into_envelope(self) -> Result<CommandEnvelope, ProtocolError> {
        let alias = match self.alias.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(_) if self.account.is_some() => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "only a new account takes an alias",
                ));
            }
            Some(text) => Some(AccountAlias::new(text)?),
        };
        Ok(CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command: Command::AuthorizeAccount {
                service: self.service,
                account: self.account,
                alias,
            },
        })
    }
}

/// What to do with an account. Added in protocol 1.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AccountAction {
    /// [`Command::SetAccountEnabled`], enabling it.
    Enable,
    /// [`Command::SetAccountEnabled`], disabling it.
    Disable,
    /// [`Command::RemoveAccount`].
    Remove,
}

/// Enables, disables, or removes an account at the revision the client
/// saw. Added in protocol 1.3.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AccountCommandRequest {
    /// Identifies the command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// What to do.
    pub action: AccountAction,
    /// The account revision the client last saw.
    pub expected_revision: Revision,
}

impl AccountCommandRequest {
    /// The command envelope for `account`.
    pub fn into_envelope(self, account: AccountId) -> CommandEnvelope {
        let expected_revision = self.expected_revision;
        let command = match self.action {
            AccountAction::Enable | AccountAction::Disable => Command::SetAccountEnabled {
                account,
                expected_revision,
                enabled: self.action == AccountAction::Enable,
            },
            AccountAction::Remove => Command::RemoveAccount {
                account,
                expected_revision,
            },
        };
        CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::SpeedLimit;
    use serde_json::json;

    fn submit(items: serde_json::Value) -> Result<CommandEnvelope, ProtocolError> {
        serde_json::from_value::<SubmitRequest>(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "items": items,
        }))
        .unwrap()
        .into_envelope()
    }

    #[test]
    fn a_bad_item_is_named_by_position() {
        let err = submit(json!(["https://a.example/1", "ftp://a.example/2"])).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.as_str().starts_with("item 2 rejected"), "{err}");
        assert_eq!(
            submit(json!([])).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let many = vec!["https://a.example/"; crate::command::MAX_SUBMIT_ITEMS + 1];
        assert_eq!(
            submit(json!(many)).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn job_commands_carry_the_expected_revision() {
        let request: JobCommandRequest = serde_json::from_value(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "action": "cancel",
            "expected_revision": 3,
        }))
        .unwrap();
        let job = JobId::from_u64(7).unwrap();
        assert_eq!(
            request.into_envelope(job).unwrap().command,
            Command::Cancel {
                job,
                expected_revision: Revision::from_u64(3).unwrap()
            }
        );
    }

    #[test]
    fn only_a_rename_carries_a_name() {
        let request = |action: &str, name: Option<&str>| {
            serde_json::from_value::<JobCommandRequest>(json!({
                "idempotency_key": IdempotencyKey::generate(),
                "target": InstanceId::generate(),
                "action": action,
                "expected_revision": 2,
                "name": name,
            }))
            .unwrap()
            .into_envelope(JobId::from_u64(1).unwrap())
        };
        assert!(matches!(
            request("rename", Some("b.bin")).unwrap().command,
            Command::Rename { name, .. } if name == "b.bin"
        ));
        for (action, name) in [("rename", None), ("remove", Some("b.bin"))] {
            assert_eq!(
                request(action, name).unwrap_err().code,
                ErrorCode::InvalidRequest
            );
        }
        assert!(matches!(
            request("remove", None).unwrap().command,
            Command::Remove { .. }
        ));
        assert!(matches!(
            request("retry", None).unwrap().command,
            Command::Retry { .. }
        ));
        assert_eq!(
            request("retry", Some("b.bin")).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn settings_requests_name_a_root_and_revision() {
        let request: UpdateSettingsRequest = serde_json::from_value(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "expected_revision": 1,
            "default_root": "root-4",
        }))
        .unwrap();
        assert_eq!(
            request.into_envelope().command,
            Command::UpdateSettings {
                expected_revision: Revision::from_u64(1).unwrap(),
                default_root: RootId::from_u64(4),
                max_active_downloads: None,
                download_speed_limit: None,
            }
        );
        let preference: UpdateSettingsRequest = serde_json::from_value(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "expected_revision": 2,
            "max_active_downloads": {"set": 6},
        }))
        .unwrap();
        assert_eq!(
            preference.into_envelope().command,
            Command::UpdateSettings {
                expected_revision: Revision::from_u64(2).unwrap(),
                default_root: None,
                max_active_downloads: Some(PreferenceChange::Set(6)),
                download_speed_limit: None,
            }
        );
        let both: UpdateSettingsRequest = serde_json::from_value(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "expected_revision": 2,
            "max_active_downloads": {"set": 6},
            "download_speed_limit": {"set": {"bytes_per_second": 1_048_576}},
        }))
        .unwrap();
        assert_eq!(
            both.download_speed_limit,
            Some(SpeedLimitChange::Set(SpeedLimit::BytesPerSecond(1_048_576)))
        );
        let reset: UpdateSettingsRequest = serde_json::from_value(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "expected_revision": 2,
            "max_active_downloads": "use_default",
        }))
        .unwrap();
        assert_eq!(
            reset.max_active_downloads,
            Some(PreferenceChange::UseDefault)
        );
        let path = serde_json::from_value::<UpdateSettingsRequest>(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "expected_revision": 1,
            "default_root": "/etc",
        }));
        assert!(path.is_err());
    }

    fn links(
        action: &str,
        links: serde_json::Value,
        destination: Option<&str>,
    ) -> Result<Command, ProtocolError> {
        serde_json::from_value::<LinksCommandRequest>(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "action": action,
            "links": links,
            "destination": destination,
        }))
        .unwrap()
        .into_envelope()
        .map(|e| e.command)
    }

    #[test]
    fn start_routing_is_explicit_and_conflicting_routes_are_rejected() {
        let request = |routing: Option<&str>, account: Option<&str>, action: &str| {
            let mut value = json!({"idempotency_key": IdempotencyKey::generate(), "target": InstanceId::generate(), "action": action, "links": [{"link": "src-1", "revision": 1}]});
            if let Some(routing) = routing {
                value["routing"] = json!(routing);
            }
            if let Some(account) = account {
                value["account"] = json!(account);
            }
            serde_json::from_value::<LinksCommandRequest>(value)
                .unwrap()
                .into_envelope()
        };
        for routing in ["direct", "automatic"] {
            assert!(request(Some(routing), None, "start").is_ok());
            assert_eq!(
                request(Some(routing), Some("acct-1"), "start")
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidRequest
            );
            assert_eq!(
                request(Some(routing), None, "remove").unwrap_err().code,
                ErrorCode::InvalidRequest
            );
        }
        let omitted = request(None, None, "start").unwrap();
        assert_eq!(
            omitted.command.required_permissions(),
            [
                crate::Permission::Submit,
                crate::Permission::ProviderDownloads
            ]
        );
        let explicit = request(Some("direct"), None, "start").unwrap();
        assert_eq!(
            explicit.command.required_permissions(),
            [crate::Permission::Submit]
        );
    }

    #[test]
    fn link_commands_name_each_link_once_at_a_revision() {
        let one = json!([{"link": "src-3", "revision": 2}]);
        let Command::StartLinks {
            links: refs,
            account: None,
            routing: None,
        } = links("start", one.clone(), None).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            refs.as_slice(),
            [LinkRef {
                link: wayfetch_domain::id::SourceLinkId::from_u64(3).unwrap(),
                revision: Revision::from_u64(2).unwrap(),
            }]
        );
        assert!(matches!(
            links("move", one.clone(), Some("root-2")).unwrap(),
            Command::MoveLinks { destination, .. } if destination == RootId::from_u64(2).unwrap()
        ));
        for (action, destination) in [("move", None), ("remove", Some("root-2"))] {
            assert_eq!(
                links(action, one.clone(), destination).unwrap_err().code,
                ErrorCode::InvalidRequest
            );
        }
        let twice = json!([{"link": "src-3", "revision": 2}, {"link": "src-3", "revision": 1}]);
        assert_eq!(
            links("remove", twice, None).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            links("start", json!([]), None).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let many: Vec<_> = (1..=crate::command::MAX_SUBMIT_ITEMS + 1)
            .map(|i| json!({"link": format!("src-{i}"), "revision": 1}))
            .collect();
        assert_eq!(
            links("start", json!(many), None).unwrap_err().code,
            ErrorCode::LimitExceeded
        );
    }

    #[test]
    fn only_a_start_names_an_account_and_only_a_retry_repeats() {
        let one = json!([{"link": "src-3", "revision": 2}]);
        let with_account = |action: &str| {
            serde_json::from_value::<LinksCommandRequest>(json!({
                "idempotency_key": IdempotencyKey::generate(),
                "target": InstanceId::generate(),
                "action": action,
                "links": one,
                "account": "acct-4",
            }))
            .unwrap()
            .into_envelope()
            .map(|e| e.command)
        };
        let started = with_account("start").unwrap();
        assert!(matches!(
            &started,
            Command::StartLinks { account: Some(a), .. } if a.get() == 4
        ));
        assert_eq!(
            started.required_permissions(),
            [
                crate::Permission::Submit,
                crate::Permission::ProviderDownloads
            ]
        );
        assert_eq!(
            with_account("remove").unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        let job = |action: &str| {
            serde_json::from_value::<JobCommandRequest>(json!({
                "idempotency_key": IdempotencyKey::generate(),
                "target": InstanceId::generate(),
                "action": action,
                "expected_revision": 2,
                "repeat_provider_operation": true,
            }))
            .unwrap()
            .into_envelope(JobId::from_u64(1).unwrap())
            .map(|e| e.command)
        };
        let retry = job("retry").unwrap();
        assert!(matches!(
            retry,
            Command::Retry {
                repeat_provider_operation: true,
                ..
            }
        ));
        assert_eq!(
            retry.required_permissions(),
            [
                crate::Permission::Control,
                crate::Permission::ProviderDownloads
            ]
        );
        assert_eq!(job("resume").unwrap_err().code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn collected_entries_are_kept_as_typed_within_limits() {
        let collect = |items: serde_json::Value| {
            serde_json::from_value::<CollectRequest>(json!({
                "idempotency_key": IdempotencyKey::generate(),
                "target": InstanceId::generate(),
                "items": items,
            }))
            .unwrap()
            .into_envelope()
        };
        // An invalid entry does not refuse the others; the engine keeps it.
        let Command::CollectLinks { items, destination } =
            collect(json!(["https://a.example/1", "not a link"]))
                .unwrap()
                .command
        else {
            panic!()
        };
        assert_eq!(items.as_slice(), ["https://a.example/1", "not a link"]);
        assert_eq!(destination, None);
        let long = "x".repeat(crate::command::MAX_URL_BYTES + 1);
        let err = collect(json!(["https://a.example/", long])).unwrap_err();
        assert_eq!(err.code, ErrorCode::LimitExceeded);
        assert!(err.message.as_str().starts_with("item 2"), "{err}");
        assert_eq!(
            collect(json!([])).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn account_requests_build_their_commands() {
        let authorize = |value: serde_json::Value| {
            let mut body = json!({
                "idempotency_key": IdempotencyKey::generate(),
                "target": InstanceId::generate(),
                "service": "real-debrid",
            });
            body.as_object_mut()
                .unwrap()
                .extend(value.as_object().unwrap().clone());
            serde_json::from_value::<AuthorizeAccountRequest>(body).map(|r| r.into_envelope())
        };
        let Command::AuthorizeAccount { alias, account, .. } =
            authorize(json!({"alias": "  Home "}))
                .unwrap()
                .unwrap()
                .command
        else {
            panic!()
        };
        assert_eq!(alias.unwrap().as_str(), "Home");
        assert_eq!(account, None);
        let blank = authorize(json!({"alias": " "})).unwrap().unwrap();
        assert!(matches!(
            blank.command,
            Command::AuthorizeAccount { alias: None, .. }
        ));
        let again = json!({"account": {"account": "acct-2", "revision": 3}, "alias": "x"});
        assert_eq!(
            authorize(again).unwrap().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        // Credentials have no field to arrive in.
        assert!(authorize(json!({"password": "hunter2"})).is_err());

        let remove: AccountCommandRequest = serde_json::from_value(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "action": "disable",
            "expected_revision": 2,
        }))
        .unwrap();
        let account = AccountId::from_u64(2).unwrap();
        assert_eq!(
            remove.into_envelope(account).command,
            Command::SetAccountEnabled {
                account,
                expected_revision: Revision::from_u64(2).unwrap(),
                enabled: false,
            }
        );
    }
}
