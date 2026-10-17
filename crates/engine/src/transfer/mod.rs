//! Transfer interfaces between the HTTP layer, the filesystem, and the
//! store (architecture brief sections 6.5, 7.5, and 7.6; ADR-09, ADR-14).
//!
//! One transfer runs like this:
//!
//! 1. An adapter's resolve step yields a [`lease::TransferOffer`]: the
//!    candidate it is bound to, what is known of the representation, a
//!    prepared request or an already-open response, an expiry, and its
//!    replay, resume, and parallelism policies.
//! 2. The coordinator issues it as a [`lease::TransferLease`] to one
//!    attempt of one job generation. Starting the transfer consumes the
//!    lease; a stale generation or an expired offer cannot start.
//! 3. Every further request (a retry, a resume, another segment) asks
//!    [`lease::Transfer::authorize`], which never clones an open response,
//!    never repeats a non-replayable request, and resumes only under a
//!    trustworthy representation that has not changed.
//! 4. Each response's parsed framing goes through [`range::plan`] before a
//!    byte is written: a matching `206` is written at its offset, a `200`
//!    to a range request is never appended, and `416` is reconciled
//!    against durable coverage, not taken as completion.
//! 5. An [`writer::IntervalWriter`] streams the body into the generation's
//!    staging file ([`crate::fs`]) within its interval, with backpressure
//!    and cancellation, and hands out synchronized checkpoints for the
//!    store to commit. Every writer of an engine takes its bytes from the
//!    one [`throttle::Bandwidth`], the total download speed limit.
//!
//! The HTTP client, redirects, egress policy, and the broker that holds
//! prepared requests and open responses live in the `wayfetch-net` crate,
//! which depends on the engine and plugs in through
//! [`transport::Transport`], yielding a [`range::Framing`] and a
//! [`writer::ChunkSource`]. Sessions and cookies are later work.

pub mod lease;
pub mod range;
pub mod throttle;
pub mod transport;
pub mod writer;
