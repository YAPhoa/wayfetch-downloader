//! Engine errors as HTTP responses.
//!
//! The body is always the protocol's [`ProtocolError`], so HTTP clients see
//! the same codes as desktop IPC clients; the status code only summarizes
//! it. Messages never repeat request input, so a rejected signed URL or
//! token cannot reach a client log through an error.

use std::time::Duration;

use axum::Json;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use wayfetch_engine::protocol::{ErrorCode, ProtocolError};

/// A request failed.
#[derive(Debug)]
pub struct ApiError {
    error: ProtocolError,
    kind: Kind,
}

/// Failures whose HTTP status is not just their code's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Status from the code alone.
    Plain,
    /// The request carried no valid credentials, as opposed to valid
    /// credentials without the needed permission.
    Unauthenticated,
    /// The request body did not arrive in time.
    TimedOut,
    /// Too many attempts; try again after this many seconds.
    RateLimited(u64),
}

impl ApiError {
    /// An error with a code and message.
    pub fn new(code: ErrorCode, message: &str) -> Self {
        ProtocolError::new(code, message).into()
    }

    /// Missing or unknown credentials: a bearer token or a live session.
    pub fn unauthenticated() -> Self {
        Self::unauthenticated_because("valid credentials are required")
    }

    /// Credentials that did not authenticate, saying no more than
    /// `message` about why, so a guesser learns nothing from it.
    pub fn unauthenticated_because(message: &str) -> Self {
        Self {
            kind: Kind::Unauthenticated,
            ..Self::new(ErrorCode::PermissionDenied, message)
        }
    }

    /// Too many attempts; another may follow after `retry_after`.
    pub fn rate_limited(retry_after: Duration) -> Self {
        // Whole seconds, rounded up, so a client that waits is admitted.
        let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
        Self {
            kind: Kind::RateLimited(seconds.max(1)),
            ..Self::new(ErrorCode::Overloaded, "too many attempts; try again later")
        }
    }

    /// A request body that did not arrive before the deadline. The
    /// connection is closed, since the rest of the body is never read.
    pub fn body_timeout() -> Self {
        Self {
            kind: Kind::TimedOut,
            ..Self::new(
                ErrorCode::InvalidRequest,
                "request body not received in time",
            )
        }
    }

    /// A body that is not valid JSON for the request type. Says where, not
    /// what, so no input is echoed.
    pub fn bad_json(error: &serde_json::Error) -> Self {
        let what = match error.classify() {
            serde_json::error::Category::Data => "does not match the request schema",
            _ => "is not valid JSON",
        };
        Self::new(
            ErrorCode::InvalidRequest,
            &format!(
                "request body {what} (line {}, column {})",
                error.line(),
                error.column()
            ),
        )
    }

    fn status(&self) -> StatusCode {
        match self.kind {
            Kind::Plain => {}
            Kind::Unauthenticated => return StatusCode::UNAUTHORIZED,
            Kind::TimedOut => return StatusCode::REQUEST_TIMEOUT,
            Kind::RateLimited(_) => return StatusCode::TOO_MANY_REQUESTS,
        }
        match self.error.code {
            ErrorCode::InvalidRequest | ErrorCode::IncompatibleProtocol => StatusCode::BAD_REQUEST,
            ErrorCode::LimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
            ErrorCode::NotFound => StatusCode::NOT_FOUND,
            ErrorCode::RevisionConflict
            | ErrorCode::NotApplicable
            | ErrorCode::IdempotencyConflict
            | ErrorCode::WrongInstance => StatusCode::CONFLICT,
            ErrorCode::Busy | ErrorCode::Overloaded | ErrorCode::ShuttingDown => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<ProtocolError> for ApiError {
    fn from(error: ProtocolError) -> Self {
        Self {
            error,
            kind: Kind::Plain,
        }
    }
}

impl From<BytesRejection> for ApiError {
    fn from(rejection: BytesRejection) -> Self {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            Self::new(ErrorCode::LimitExceeded, "request body too large")
        } else {
            Self::new(ErrorCode::InvalidRequest, "request body could not be read")
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, Json(self.error)).into_response();
        let headers = response.headers_mut();
        match self.kind {
            Kind::Unauthenticated => {
                headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            Kind::TimedOut => {
                headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
            }
            Kind::RateLimited(seconds) => {
                headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
            }
            Kind::Plain if status == StatusCode::SERVICE_UNAVAILABLE => {
                headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            }
            Kind::Plain => {}
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_status_and_retryable_ones_say_when() {
        let status = |code| ApiError::new(code, "x").into_response().status();
        assert_eq!(status(ErrorCode::RevisionConflict), StatusCode::CONFLICT);
        assert_eq!(
            status(ErrorCode::LimitExceeded),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(status(ErrorCode::PermissionDenied), StatusCode::FORBIDDEN);
        let busy = ApiError::new(ErrorCode::Overloaded, "x").into_response();
        assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(busy.headers()[header::RETRY_AFTER], "1");

        let denied = ApiError::unauthenticated().into_response();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(denied.headers()[header::WWW_AUTHENTICATE], "Bearer");

        let limited = ApiError::rate_limited(Duration::from_millis(11_500)).into_response();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.headers()[header::RETRY_AFTER], "12");
        let soon = ApiError::rate_limited(Duration::ZERO).into_response();
        assert_eq!(soon.headers()[header::RETRY_AFTER], "1");
    }

    #[test]
    fn json_errors_do_not_echo_input() {
        let secret = r#"{"items": ["https://example.test/?sig=SECRET"], "x": 1"#;
        let error = serde_json::from_str::<serde_json::Value>(secret).unwrap_err();
        let message = ApiError::bad_json(&error).error.message.to_string();
        assert!(!message.contains("SECRET"), "{message}");

        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code, reason = "only the error matters")]
        struct Wants {
            count: u8,
        }
        let error = serde_json::from_str::<Wants>(r#"{"count": "SECRET"}"#).unwrap_err();
        let message = ApiError::bad_json(&error).error.message.to_string();
        assert!(message.contains("schema") && !message.contains("SECRET"));
    }
}
