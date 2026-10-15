//! Request bodies that clients send for commands, and their translation to
//! [`CommandEnvelope`]s.
//!
//! The browser posts them over HTTP and the desktop shell passes them over
//! IPC, so both transports validate inputs, report a bad item by its
//! position, and build the same envelope (architecture brief section 4.7,
//! test T62).

use serde::Deserialize;
use wayfetch_domain::id::{JobId, PackageId, RootId};
use wayfetch_domain::version::Revision;

use crate::command::{Command, CommandEnvelope, IdempotencyKey, InputUrl, SubmitItems};
use crate::error::{ErrorCode, ProtocolError};
use crate::instance::InstanceId;

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
}

/// Pauses, resumes, cancels, renames, or removes a job at the revision the
/// client saw.
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
}

impl JobCommandRequest {
    /// The command envelope for `job`. A name is required for a rename and
    /// refused with any other action.
    pub fn into_envelope(self, job: JobId) -> Result<CommandEnvelope, ProtocolError> {
        let expected_revision = self.expected_revision;
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
            (Action::Remove, None) => Command::Remove {
                job,
                expected_revision,
            },
        };
        Ok(CommandEnvelope {
            idempotency_key: self.idempotency_key,
            target: self.target,
            command,
        })
    }
}

/// Chooses the default destination at the settings revision the client
/// saw.
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
    /// Where jobs submitted from now on are saved.
    pub default_root: RootId,
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
            },
        }
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

#[cfg(test)]
mod tests {
    use super::*;
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
                default_root: RootId::from_u64(4).unwrap(),
            }
        );
        let path = serde_json::from_value::<UpdateSettingsRequest>(json!({
            "idempotency_key": IdempotencyKey::generate(),
            "target": InstanceId::generate(),
            "expected_revision": 1,
            "default_root": "/etc",
        }));
        assert!(path.is_err());
    }
}
