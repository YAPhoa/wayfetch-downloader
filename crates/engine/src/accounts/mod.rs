//! Provider accounts as an engine-owned service (architecture brief
//! sections 3.11, 7.3, 8.3, 8.4, ADR-24).
//!
//! The engine owns everything about an account except its provider's wire
//! protocol: the account records and their revisions, the encrypted
//! credentials and their key, pending authorizations, single-flight status
//! checks, and the policy for what each provider answer means. A host
//! supplies the [`provider::AccountProvider`]s it ships, each built on an
//! [`provider::ApiClient`] that enforces the network policy.
//!
//! * [`secret`]: bytes that never print or serialize.
//! * [`provider`]: the seams to a provider and to its API client.
//! * `vault`: the key file, sealing, and locked startup.
//!
//! Credentials never come from a client: a person approves Wayfetch on the
//! provider's own site, and the provider hands the engine the credentials.
//! Clients see only redacted summaries
//! ([`wayfetch_protocol::account::AccountSummary`]).

pub mod provider;
pub mod secret;
pub(crate) mod vault;

pub use secret::Secret;
