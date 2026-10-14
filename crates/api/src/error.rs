//! Engine errors as HTTP responses.
//!
//! The body is always the protocol's [`ProtocolError`], so HTTP clients see
//! the same codes as desktop IPC clients; the status code only summarizes
//! it. Messages never repeat request input, so a rejected signed URL or
//! token cannot reach a client log through an error.

use axum::Json;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use wayfetch_engine::protocol::{ErrorCode, ProtocolError};

/// A request failed.
#[derive(Debug)]
pub struct ApiError {
    error: ProtocolError,
    /// The request carried no valid credentials, as opposed to valid
    /// credentials without the needed permission.
    unauthenticated: bool,
}

impl ApiError {
    /// An error with a code and message.
    pub fn new(code: ErrorCode, message: &str) -> Self {
        ProtocolError::new(code, message).into()
    }

    /// Missing or unknown credentials.
    pub fn unauthenticated() -> Self {
        Self {
            unauthenticated: true,
            ..Self::new(
                ErrorCode::PermissionDenied,
                "a valid bearer token is required",
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
        if self.unauthenticated {
            return StatusCode::UNAUTHORIZED;
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
            unauthenticated: false,
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
        if self.unauthenticated {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        } else if status == StatusCode::SERVICE_UNAVAILABLE {
            headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
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
