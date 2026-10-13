//! Domain identifiers, states, and validated types for Wayfetch Downloader.
//!
//! This crate has no I/O, runtime, or transport dependencies. With the
//! `serde` feature its types serialize for the transport-neutral protocol.
//! It holds the types every other crate agrees on (architecture brief sections 3.4, 3.5,
//! 6.1, 7.1 and 7.4):
//!
//! * [`id`]: typed, opaque record identifiers;
//! * [`version`]: revisions for concurrent edits and generations for
//!   invalidating stale work;
//! * [`source`]: resource identity and the host/provider/account/route
//!   candidate tuple;
//! * [`job`]: the file-job state machine;
//! * [`failure`]: typed failure codes, scopes, and next actions;
//! * [`name`] and [`time`]: validated names and stored timestamps.

pub mod failure;
pub mod id;
pub mod job;
pub mod name;
#[cfg(all(test, feature = "serde"))]
mod serde_tests;
pub mod source;
pub mod time;
pub mod version;
