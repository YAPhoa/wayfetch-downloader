//! Transfer interfaces between the HTTP layer, the filesystem, and the store
//! (ADR-09, ADR-14).
//!
//! An adapter's offer becomes a [`lease::TransferLease`] for one job
//! generation; every further request is authorized by the lease, each
//! response's framing is checked by [`range::plan`] before a byte is
//! written, and an [`writer::IntervalWriter`] streams the body into staging
//! under the shared [`throttle::Bandwidth`]. The HTTP side lives in
//! `wayfetch-net` behind [`transport::Transport`].

pub mod lease;
pub mod range;
pub mod throttle;
pub mod transport;
pub mod writer;
