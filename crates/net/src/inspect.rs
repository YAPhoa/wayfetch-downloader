//! Link checks' metadata requests: the engine's [`Inspector`] over the same
//! client, redirect rules, and egress policy as transfers (link crawler
//! stage 1).
//!
//! A check asks with `HEAD`, a first-byte `GET`, or a `GET` whose body is
//! read only when the answer is a web page, up to the request's limit. A
//! file's body is never read: the response is dropped after its headers.
//! These requests may be repeated, so redirects are followed like a direct
//! download's. Errors never carry the URL, which may hold a token.

use bytes::BytesMut;
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use wayfetch_engine::crawl::{
    InspectError, InspectMethod, InspectRequest, Inspection, Inspector, is_page,
};
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::transport::TransportError;
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::head;
use crate::policy::EgressPolicy;
use crate::transport::{HttpTransport, Outgoing};

fn fail(reason: CheckReason, message: impl Into<String>) -> InspectError {
    InspectError {
        reason,
        message: message.into(),
    }
}

/// What a request that got no answer says to a check: the reason, and
/// the transport's message only for TLS, where it says what was wrong.
/// Other messages are the HTTP client's, which mean nothing to a person.
fn failed(e: TransportError) -> InspectError {
    let reason = match e.code {
        FailureCode::UnsupportedCapability if e.message.starts_with("TLS") => {
            CheckReason::Unreachable
        }
        FailureCode::UnsupportedCapability if e.message.contains("redirect") => {
            CheckReason::Unreadable
        }
        // The egress policy refused the address.
        FailureCode::UnsupportedCapability => CheckReason::Blocked,
        FailureCode::RequestTimeout if e.message.contains("timed out") => CheckReason::TimedOut,
        FailureCode::RequestTimeout => CheckReason::Unreachable,
        _ => CheckReason::Unreadable,
    };
    let tls = e.message.starts_with("TLS");
    fail(reason, if tls { e.message } else { String::new() })
}

impl HttpTransport {
    async fn inspection(&self, request: &InspectRequest) -> Result<Inspection, InspectError> {
        let (method, range) = match request.method {
            InspectMethod::Head => (Method::HEAD, None),
            InspectMethod::FirstByte => (Method::GET, Some((0, Some(1)))),
            InspectMethod::Page => (Method::GET, None),
        };
        let (mut response, url) = self
            .send(Outgoing {
                method,
                url: request.url.clone(),
                headers: HeaderMap::new(),
                body: None,
                range,
                validators: None,
                connect_timeout: request.deadline,
                reach: request.reach,
                replay_safe: true,
            })
            .await
            .map_err(failed)?;
        let status = response.status();
        let described = head::describe(status, response.headers());
        let mut inspection = Inspection {
            status: status.as_u16(),
            final_url: Some(url),
            content_type: described.content_type,
            length: described.length,
            filename: described.filename,
            attachment: described.attachment,
            page: None,
            // Unknown counts as public, so links found on it stay public.
            local: response
                .remote_addr()
                .is_some_and(|peer| !EgressPolicy::default().permits(peer.ip())),
        };
        let page = request.method == InspectMethod::Page
            && status == StatusCode::OK
            && is_page(inspection.content_type.as_deref(), inspection.attachment);
        if !page {
            // A file's body is never read: dropping the response closes it.
            return Ok(inspection);
        }
        if described.encoded {
            return Err(fail(
                CheckReason::Unreadable,
                "The page was sent compressed, which Wayfetch does not read.",
            ));
        }
        let too_large = || {
            fail(
                CheckReason::TooLarge,
                format!(
                    "The page is larger than the {} KiB Wayfetch reads.",
                    request.max_page / 1024
                ),
            )
        };
        if response
            .content_length()
            .is_some_and(|n| n > request.max_page as u64)
        {
            return Err(too_large());
        }
        let mut body = BytesMut::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            fail(
                CheckReason::Unreachable,
                format!("The page stopped arriving: {}", e.without_url()),
            )
        })? {
            if body.len() + chunk.len() > request.max_page {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        inspection.page = Some(body.freeze());
        Ok(inspection)
    }
}

impl Inspector for HttpTransport {
    fn inspect<'a>(
        &'a self,
        request: &'a InspectRequest,
    ) -> BoxFuture<'a, Result<Inspection, InspectError>> {
        Box::pin(async move {
            tokio::time::timeout(request.deadline, self.inspection(request))
                .await
                .unwrap_or_else(|_| Err(fail(CheckReason::TimedOut, "No answer in time.")))
        })
    }
}
