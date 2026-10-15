//! Request bodies that clients send for commands, and their translation to
//! [`CommandEnvelope`]s.
//!
//! The browser posts them over HTTP and the desktop shell passes them over
//! IPC, so both transports validate inputs, report a bad item by its
//! position, and build the same envelope (architecture brief section 4.7,
//! test T62).

use serde::Deserialize;
use wayfetch_domain::id::{JobId, PackageId};
use wayfetch_domain::version::Revision;

use crate::command::{Command, CommandEnvelope, IdempotencyKey, InputUrl, SubmitItems};
use crate::error::ProtocolError;
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
}

/// Pauses, resumes, or cancels a job at the revision the client saw.
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
}

impl JobCommandRequest {
    /// The command envelope for `job`.
    pub fn into_envelope(self, job: JobId) -> CommandEnvelope {
        let expected_revision = self.expected_revision;
        let command = match self.action {
            Action::Pause => Command::Pause {
                job,
                expected_revision,
            },
            Action::Resume => Command::Resume {
                job,
                expected_revision,
            },
            Action::Cancel => Command::Cancel {
                job,
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
    use crate::ErrorCode;
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
            request.into_envelope(job).command,
            Command::Cancel {
                job,
                expected_revision: Revision::from_u64(3).unwrap()
            }
        );
    }
}
