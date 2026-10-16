//! The HTTP layer of Wayfetch Downloader: an HTTP(S) [`HttpTransport`] the
//! host hands to the engine, and the broker that holds prepared requests
//! and open responses for transfer leases (architecture brief sections 6.5,
//! 7.5, and 8.2). The same transport is the engine's API client for
//! account integrations' metadata requests.
//!
//! The engine decides what to request and whether a response may be
//! written ([`wayfetch_engine::transfer`]); this crate sends requests under
//! a fixed transfer policy and reports what came back. It depends on the
//! engine, never the other way round, so the engine builds and embeds
//! without an HTTP client.

mod api;
mod body;
mod head;
mod policy;
mod transport;

pub use head::ResponseHead;
pub use policy::{EgressPolicy, MAX_URL_BYTES};
pub use rustls::pki_types::CertificateDer;
pub use transport::{
    BrokerError, BuildError, HttpConfig, HttpTransport, LeaseRequest, PreparedRequest,
};
pub use {ipnet, reqwest::Method, reqwest::header};
