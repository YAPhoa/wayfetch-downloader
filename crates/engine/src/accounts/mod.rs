//! Provider accounts as an engine-owned service (ADR-24).
//!
//! The engine owns account records, encrypted credentials, pending sign-ins,
//! and status checks; hosts supply the [`provider::AccountProvider`]s they
//! ship. Credentials come only from the provider after the person approves
//! on its site, and clients see only redacted summaries.

pub mod provider;
pub mod secret;
pub(crate) mod vault;

pub use secret::Secret;
