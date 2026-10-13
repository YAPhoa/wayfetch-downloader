//! The reusable Wayfetch Downloader engine library.
//!
//! Application hosts (`wayfetchd`, the desktop shell, test harnesses) compose
//! the engine; it never owns an HTTP listener, window, process-wide signal
//! handler, or frontend asset build (architecture brief sections 4.5 and 4.6).
//!
//! A host starts an engine with [`EngineBuilder`] on its own Tokio runtime,
//! keeps the [`EngineOwner`], hands [`EngineHandle`]s to its adapters, and
//! awaits [`EngineOwner::shutdown`] before exiting.

pub mod config;
pub mod lifecycle;
pub mod state_dir;

pub use config::EngineConfig;
pub use lifecycle::{
    EngineBuilder, EngineHandle, EngineOwner, Readiness, ShutdownReport, ShutdownRequest,
    StartError,
};
pub use wayfetch_domain as domain;
pub use wayfetch_protocol as protocol;
