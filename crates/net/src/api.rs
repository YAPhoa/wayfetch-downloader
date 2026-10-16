//! Provider API requests for account integrations: the engine's
//! [`ApiClient`] over the same client, TLS, and egress policy as transfers
//! (architecture brief sections 6.5 and 8.2, ADR-15).
//!
//! A metadata request differs from a transfer: it is sent once, its whole
//! body is read within a size limit and a deadline, and nothing about it is
//! resumable. So redirects are not followed at all (a provider API that
//! redirects is answered as refused, and no credential can follow a hop),
//! the body must not be encoded, and the bearer token and form body are
//! marked sensitive. Errors are built from the client's error kinds without
//! the URL, which may carry a provider's device code.

use std::time::Duration;

use reqwest::header::{self, HeaderValue};
use reqwest::{Method, StatusCode};
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    ApiClient, ApiError, ApiErrorKind, ApiMethod, ApiRequest, ApiResponse,
};
use wayfetch_engine::transfer::writer::BoxFuture;
use zeroize::Zeroizing;

use crate::policy::Refused;
use crate::transport::{HttpTransport, find};

/// Longest wait for a connection, within the request's deadline.
const MAX_CONNECT: Duration = Duration::from_secs(30);

fn fail(kind: ApiErrorKind, message: impl Into<String>) -> ApiError {
    ApiError {
        kind,
        message: message.into(),
    }
}

impl HttpTransport {
    async fn exchange(&self, request: &ApiRequest) -> Result<ApiResponse, ApiError> {
        if !matches!(request.url.scheme(), "https" | "http") {
            return Err(fail(
                ApiErrorKind::Refused,
                "only HTTP(S) APIs are supported",
            ));
        }
        self.policy
            .check(&request.url)
            .map_err(|Refused(why)| fail(ApiErrorKind::Refused, why))?;
        let client = self
            .client(request.deadline.min(MAX_CONNECT))
            .map_err(|e| fail(ApiErrorKind::Unreachable, e.message))?;
        let method = match request.method {
            ApiMethod::Get => Method::GET,
            ApiMethod::Post => Method::POST,
        };
        let mut builder = client
            .request(method, request.url.clone())
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity");
        if let Some(token) = &request.bearer {
            let mut value = Zeroizing::new(b"Bearer ".to_vec());
            value.extend_from_slice(token.expose());
            let mut value = HeaderValue::from_bytes(&value).map_err(|_| {
                fail(
                    ApiErrorKind::Refused,
                    "the token cannot be sent in a header",
                )
            })?;
            value.set_sensitive(true);
            builder = builder.header(header::AUTHORIZATION, value);
        }
        if let Some(form) = &request.form {
            builder = builder
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(form.expose().to_vec());
        }
        let mut response = builder.send().await.map_err(send_failure)?;
        let status = response.status();
        if status.is_redirection() && status != StatusCode::NOT_MODIFIED {
            return Err(fail(
                ApiErrorKind::Unreadable,
                format!("HTTP {status}: provider API redirects are not followed"),
            ));
        }
        let encoded = response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| !v.as_bytes().eq_ignore_ascii_case(b"identity"));
        if encoded {
            return Err(fail(
                ApiErrorKind::Unreadable,
                "the response body is encoded",
            ));
        }
        let too_large = || {
            fail(
                ApiErrorKind::TooLarge,
                format!("the response is larger than {} bytes", request.max_body),
            )
        };
        if response
            .content_length()
            .is_some_and(|n| n > request.max_body as u64)
        {
            return Err(too_large());
        }
        let mut body = Zeroizing::new(Vec::new());
        // The request was sent: a failure now loses the provider's answer.
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            let mut e = send_failure(e);
            if e.kind == ApiErrorKind::Unreachable {
                e.kind = ApiErrorKind::Interrupted;
            }
            e
        })? {
            if body.len() + chunk.len() > request.max_body {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(ApiResponse {
            status: status.as_u16(),
            body: Secret::new(std::mem::take(&mut *body)),
        })
    }
}

impl ApiClient for HttpTransport {
    fn send<'a>(&'a self, request: &'a ApiRequest) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
        Box::pin(async move {
            tokio::time::timeout(request.deadline, self.exchange(request))
                .await
                .unwrap_or_else(|_| Err(fail(ApiErrorKind::Timeout, "no answer in time")))
        })
    }
}

/// Classifies a failed exchange, without its URL.
fn send_failure(err: reqwest::Error) -> ApiError {
    let err = err.without_url();
    if let Some(Refused(why)) = find::<Refused>(&err) {
        return fail(ApiErrorKind::Refused, why.clone());
    }
    if let Some(tls) = find::<rustls::Error>(&err) {
        return fail(
            ApiErrorKind::Refused,
            format!("TLS verification failed: {tls}"),
        );
    }
    // Only a failed connection proves the provider never saw the request.
    let kind = if err.is_timeout() {
        ApiErrorKind::Timeout
    } else if err.is_connect() {
        ApiErrorKind::Unreachable
    } else {
        ApiErrorKind::Interrupted
    };
    let detail = match std::error::Error::source(&err) {
        Some(source) => format!("{err}: {source}"),
        None => err.to_string(),
    };
    fail(kind, detail)
}
