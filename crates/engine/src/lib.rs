//! The reusable Wayfetch Downloader engine library.
//!
//! Hosts compose the engine; it never owns a listener, window, signal
//! handler, or UI build. A host starts it with [`EngineBuilder`] on its own
//! runtime, keeps the [`EngineOwner`], hands out [`EngineHandle`]s, and
//! awaits [`EngineOwner::shutdown`].

/// Marks a named crash-injection boundary. With the `failpoints` feature a
/// test can stop the process here through the `fail` crate and kill it;
/// otherwise it compiles to nothing.
macro_rules! crash_point {
    ($name:literal) => {
        #[cfg(feature = "failpoints")]
        fail::fail_point!($name);
    };
}

pub mod accounts;
mod cleanup;
pub mod config;
pub mod crawl;
mod destinations;
mod download;
pub mod extract;
pub mod fs;
pub mod lifecycle;
pub mod media;
mod preferences;
mod progress;
mod published;
pub mod state_dir;
mod store;
pub mod subscription;
pub mod transfer;

pub use config::{
    ConfigError, EngineConfig, IntegerPolicy, Limits, Policy, Preference, SpeedLimitPolicy,
    Timeouts,
};
pub use destinations::{RootError, initialize_new_root, initialize_root};
#[cfg(feature = "failpoints")]
pub use fail;
pub use lifecycle::{
    EngineBuilder, EngineHandle, EngineOwner, Readiness, ShutdownReport, ShutdownRequest,
    StartError,
};
pub use store::{StoreError, StoreInfo};
pub use subscription::Subscription;
pub use wayfetch_domain as domain;
pub use wayfetch_protocol as protocol;
