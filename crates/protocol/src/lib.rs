//! Transport-neutral commands, queries, errors, and events.
//!
//! Shared by the HTTP adapter and the desktop IPC adapter. It must not depend
//! on any host, HTTP framework, or UI toolkit (architecture brief section 4.5).
//! The wire form is JSON via serde; both transports carry these types
//! unchanged, so they share validation, idempotency, revision checks, error
//! codes, and resynchronization (test T62).
//!
//! * [`instance`]: instance identity, protocol version, and the handshake;
//! * [`command`]: idempotent mutations and their results;
//! * [`query`]: bounded, paginated reads;
//! * [`request`]: command request bodies clients send, shared by transports;
//! * [`event`]: durable events, progress samples, and resync rules;
//! * [`error`]: structured errors.

pub mod command;
pub mod error;
pub mod event;
pub mod instance;
pub mod query;
pub mod request;

pub use command::{Command, CommandEnvelope, CommandResult, IdempotencyKey, Permission};
pub use error::{ErrorCode, ProtocolError};
pub use event::{EventCursor, StreamItem, Subscribe};
pub use instance::{Hello, InstanceId, ProtocolVersion, TransportLimits};
pub use query::{Page, Query, QueryResult};
pub use wayfetch_domain as domain;
