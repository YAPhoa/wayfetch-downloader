//! Domain identifiers, states, and validated types shared by every Wayfetch
//! crate.
//!
//! No I/O, runtime, or transport dependencies; the `serde` feature makes the
//! types serializable for the protocol.

pub mod failure;
pub mod id;
pub mod job;
pub mod name;
#[cfg(all(test, feature = "serde"))]
mod serde_tests;
pub mod source;
pub mod time;
pub mod version;
