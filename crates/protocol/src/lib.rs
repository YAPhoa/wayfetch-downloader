//! Transport-neutral commands, queries, errors, and events, shared by the
//! HTTP and desktop IPC adapters.
//!
//! The wire form is JSON via serde, carried unchanged by both transports so
//! they behave the same. This crate depends on no host, HTTP framework, or
//! UI toolkit.

pub mod account;
pub mod command;
pub mod error;
pub mod event;
pub mod instance;
pub mod media;
pub mod query;
pub mod request;

pub use command::{Command, CommandEnvelope, CommandResult, IdempotencyKey, Permission};
pub use error::{ErrorCode, ProtocolError};
pub use event::{EventCursor, StreamItem, Subscribe};
pub use instance::{Hello, InstanceId, ProtocolVersion, TransportLimits};
pub use query::{Page, Query, QueryResult};
pub use wayfetch_domain as domain;
