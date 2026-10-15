//! Structured errors shared by every transport.

use serde::{Deserialize, Serialize};
use wayfetch_domain::job::JobError;
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;

/// Stable error codes. Transports map them to their own status codes (for
/// example HTTP 409 for a conflict) but always carry the code itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ErrorCode {
    /// The request is malformed or fails validation.
    InvalidRequest,
    /// The request exceeds a published [`crate::TransportLimits`] value.
    LimitExceeded,
    /// The caller lacks the required [`crate::Permission`].
    PermissionDenied,
    /// The target record does not exist.
    NotFound,
    /// The record changed since the revision the client presented; the
    /// error carries the current revision. Re-read and decide again.
    RevisionConflict,
    /// The request does not apply in the record's current state.
    NotApplicable,
    /// The record is in a short non-interruptible step, such as publishing.
    /// Retry the same request later.
    Busy,
    /// The name is taken: another file has it, or another job reserved it.
    /// Nothing was replaced. Added in protocol 1.0.
    AlreadyExists,
    /// The idempotency key was already used for a different command.
    IdempotencyConflict,
    /// The command targets a different engine instance.
    WrongInstance,
    /// The client's protocol version is not served.
    IncompatibleProtocol,
    /// A bounded queue is full. Retry later.
    Overloaded,
    /// The engine is shutting down and admits no new commands.
    ShuttingDown,
    /// An engine fault. Details are in the engine's diagnostics, not here.
    Internal,
}

impl ErrorCode {
    /// Whether retrying the identical request later may succeed.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Busy | Self::Overloaded | Self::ShuttingDown)
    }
}

/// An error returned to a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProtocolError {
    /// Stable code.
    pub code: ErrorCode,
    /// Human-readable, redacted explanation.
    pub message: DiagnosticText,
    /// The record's current revision, for [`ErrorCode::RevisionConflict`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<Revision>,
    /// When a refused retry may be asked for again, for a job whose
    /// failure has a retry deadline still in the future. Added in protocol
    /// 1.2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<UtcTimestamp>,
}

impl ProtocolError {
    /// An error with a message.
    pub fn new(code: ErrorCode, message: &str) -> Self {
        Self {
            code,
            message: DiagnosticText::new(message),
            current_revision: None,
            retry_at: None,
        }
    }
}

impl From<JobError> for ProtocolError {
    /// Maps a rejected job request. Errors that only coordinator reports can
    /// produce (stale generation, invalid transition) are engine faults from
    /// a client's point of view.
    fn from(err: JobError) -> Self {
        let code = match err {
            JobError::RevisionConflict { .. } => ErrorCode::RevisionConflict,
            JobError::NotApplicable { .. } => ErrorCode::NotApplicable,
            JobError::PublicationInProgress => ErrorCode::Busy,
            JobError::StaleGeneration { .. }
            | JobError::InvalidTransition { .. }
            | JobError::DesiredStateMismatch { .. }
            | JobError::Inconsistent { .. }
            | JobError::CounterExhausted(_) => ErrorCode::Internal,
        };
        let mut out = Self::new(code, &err.to_string());
        if let JobError::RevisionConflict { current, .. } = err {
            out.current_revision = Some(current);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wayfetch_domain::job::JobState;

    #[test]
    fn revision_conflict_carries_current_revision() {
        let mut job = JobState::new();
        let stale = job.revision();
        job.request_pause(stale).unwrap();
        let err: ProtocolError = job.request_resume(stale).unwrap_err().into();
        assert_eq!(err.code, ErrorCode::RevisionConflict);
        assert_eq!(err.current_revision, Some(job.revision()));
        let wire = serde_json::to_value(&err).unwrap();
        assert_eq!(wire["code"], json!("revision_conflict"));
        assert_eq!(wire["current_revision"], json!(2));
    }

    #[test]
    fn publication_in_progress_is_retryable() {
        let err = ProtocolError::from(JobError::PublicationInProgress);
        assert_eq!(err.code, ErrorCode::Busy);
        assert!(err.code.is_retryable());
        assert!(!ErrorCode::RevisionConflict.is_retryable());
    }

    #[test]
    fn optional_fields_are_omitted() {
        let wire =
            serde_json::to_value(ProtocolError::new(ErrorCode::NotFound, "no such job")).unwrap();
        assert_eq!(wire, json!({"code": "not_found", "message": "no such job"}));
    }
}
