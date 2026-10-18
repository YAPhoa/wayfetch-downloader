//! The seam between the engine and an HTTP client (sections 7.5 and 10.4).
//!
//! The engine decides what to request and whether a response may be
//! written; a [`Transport`] only sends the request and parses the response
//! into [`Framing`] and a [`Representation`], including the resource that
//! finally served it. The HTTP client, redirects, TLS, and status
//! classification live behind it, so tests run the whole engine against an
//! in-process transport and the host supplies a real one (`wayfetch-net`).

use std::time::Duration;

use url::Url;
use wayfetch_domain::failure::FailureCode;

use super::lease::Representation;
use super::range::Framing;
use super::writer::{BoxFuture, ChunkSource};

/// A request for a direct link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// The URL.
    pub url: Url,
    /// `None` for the whole representation, else the half-open byte
    /// interval to request, with an open end for `bytes=start-`.
    pub range: Option<(u64, Option<u64>)>,
    /// The representation a range must still belong to. The transport makes
    /// the range conditional on it (`If-Range` with its validators), so a
    /// changed resource answers with the whole new representation instead
    /// of bytes that would be spliced into the old one.
    pub validators: Option<Representation>,
    /// Deadline for establishing a connection.
    pub connect_timeout: Duration,
    /// Largest chunk the body may yield at once.
    pub read_buffer: usize,
    /// A `User-Agent` to send instead of the transport's own, for a link
    /// made for one, such as a media stream's.
    pub user_agent: Option<String>,
}

/// A response whose headers arrived.
pub struct Response {
    /// How the status and `Content-Range` frame the body.
    pub framing: Framing,
    /// What the response says about the representation. Its length is the
    /// complete length, not the length of a partial body.
    pub representation: Representation,
    /// A file name the server suggested (`Content-Disposition`), if any.
    pub filename: Option<String>,
    /// The body.
    pub body: Box<dyn ChunkSource>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("framing", &self.framing)
            .field("representation", &self.representation)
            .field("filename", &self.filename)
            .finish_non_exhaustive()
    }
}

/// Why a request produced no usable response. The transport classifies it,
/// because only it knows what a status or socket error means.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message}", code.as_str())]
pub struct TransportError {
    /// The failure to record.
    pub code: FailureCode,
    /// A short description for diagnostics, without secrets.
    pub message: String,
}

/// Sends requests for the engine.
pub trait Transport: Send + Sync + 'static {
    /// Sends `request` and returns once the response headers arrived. The
    /// engine bounds the whole call with its response deadline.
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>>;
}
