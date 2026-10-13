//! Transport-neutral commands, queries, errors, and events.
//!
//! Shared by the HTTP adapter and the desktop IPC adapter. It must not depend
//! on any host, HTTP framework, or UI toolkit (architecture brief section 4.5).

pub use wayfetch_domain as domain;
