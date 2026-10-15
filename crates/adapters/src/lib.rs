//! Provider integrations for the Wayfetch Downloader engine (architecture
//! brief sections 3.11 and 6.4, ADR-08).
//!
//! Each integration speaks one provider's protocol through the engine's
//! seams and keeps no state of its own: the engine owns accounts, stored
//! credentials, pending authorizations, and when to call. Requests go
//! through the host's [`wayfetch_engine::accounts::provider::ApiClient`],
//! so the network policy is enforced in one place.
//!
//! * [`real_debrid`]: Real-Debrid account sign-in and status. Downloading
//!   through Real-Debrid is not implemented yet.

pub mod real_debrid;
