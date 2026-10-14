//! The reusable Wayfetch Downloader engine library.
//!
//! Application hosts (`wayfetchd`, the desktop shell, test harnesses) compose
//! the engine; it never owns an HTTP listener, window, process-wide signal
//! handler, or frontend asset build (architecture brief sections 4.5 and 4.6).
//!
//! A host starts an engine with [`EngineBuilder`] on its own Tokio runtime,
//! keeps the [`EngineOwner`], hands [`EngineHandle`]s to its adapters, and
//! awaits [`EngineOwner::shutdown`] before exiting.

/// Marks a named crash-injection boundary (architecture brief section 13.2).
/// With the `failpoints` feature a test can stop the process here through
/// the `fail` crate and kill it; otherwise it compiles to nothing.
macro_rules! crash_point {
    ($name:literal) => {
        #[cfg(feature = "failpoints")]
        fail::fail_point!($name);
    };
}

pub mod config;
mod download;
pub mod fs;
pub mod lifecycle;
pub mod state_dir;
mod store;
pub mod subscription;
pub mod transfer;

pub use config::{ConfigError, EngineConfig, Limits, RootConfig, Timeouts};
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
