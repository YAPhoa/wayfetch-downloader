//! The reusable Wayfetch Downloader engine library.
//!
//! Application hosts (`wayfetchd`, the desktop shell, test harnesses) compose
//! the engine; it never owns an HTTP listener, window, process-wide signal
//! handler, or frontend asset build (architecture brief sections 4.5 and 4.6).

pub use wayfetch_domain as domain;
pub use wayfetch_protocol as protocol;
