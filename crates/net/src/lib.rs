//! The HTTP layer: the [`HttpTransport`] a host hands to the engine, also
//! used as its provider API client and link inspector.
//!
//! The engine decides what may be requested and written; this crate sends
//! requests under a fixed policy and reports what came back. It depends on
//! the engine, never the other way round.

mod api;
mod body;
mod head;
mod inspect;
mod policy;
mod transport;

pub use head::ResponseHead;
pub use policy::{EgressPolicy, MAX_URL_BYTES};
pub use rustls::pki_types::CertificateDer;
pub use transport::{
    BrokerError, BuildError, HttpConfig, HttpTransport, LeaseRequest, PreparedRequest,
};
pub use {ipnet, reqwest::Method, reqwest::header};
