//! Engine instance identity and the client handshake.
//!
//! A client learns the instance, boot epoch, and protocol compatibility
//! before any change, and commands name their target instance, so
//! reconnecting to a different engine is refused rather than acting on the
//! wrong queue.

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wayfetch_domain::version::BootEpoch;

use crate::event::EventCursor;

/// Identity of one engine state directory. Created once with the state
/// directory and never reused; it survives restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct InstanceId(Uuid);

impl InstanceId {
    /// A fresh random identity, for a newly created state directory.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps a stored identity.
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The stored identity.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Protocol version.
///
/// A minor version may only add optional fields and new commands or
/// queries. Any new value a client must decode (an event kind, error code,
/// stage, and so on) needs a new major version. Before 1.0, as in Cargo,
/// the minor version is the incompatible one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProtocolVersion {
    /// Incompatible changes.
    pub major: u16,
    /// Backward-compatible additions.
    pub minor: u16,
}

impl ProtocolVersion {
    /// The version this crate implements.
    pub const CURRENT: Self = Self { major: 0, minor: 3 };

    /// Whether an engine speaking `self` can serve a client speaking
    /// `client`: same major version, and the client needs nothing newer than
    /// the engine provides; before 1.0, the same version.
    pub const fn serves(self, client: Self) -> bool {
        if self.major == 0 {
            return client.major == 0 && client.minor == self.minor;
        }
        self.major == client.major && client.minor <= self.minor
    }
}

/// Size limits a client must respect. Requests over a limit are rejected
/// with [`crate::ErrorCode::LimitExceeded`], never truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct TransportLimits {
    /// Maximum items per page.
    pub max_page_size: u16,
    /// Maximum inputs per submit command.
    pub max_submit_items: u16,
    /// Maximum bytes of one input URL.
    pub max_url_bytes: u32,
    /// Maximum links waiting in the collected list.
    #[serde(default)]
    pub max_collected_links: u32,
    /// Maximum bytes of text one collect may carry.
    #[serde(default)]
    pub max_collect_text_bytes: u32,
}

impl TransportLimits {
    /// Limits of this protocol version.
    pub const DEFAULT: Self = Self {
        max_page_size: crate::query::PageSize::MAX.get(),
        max_submit_items: crate::command::MAX_SUBMIT_ITEMS as u16,
        max_url_bytes: crate::command::MAX_URL_BYTES as u32,
        max_collected_links: crate::command::MAX_COLLECTED_LINKS as u32,
        max_collect_text_bytes: crate::command::MAX_COLLECT_TEXT_BYTES as u32,
    };
}

/// The first message on every connection and subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Hello {
    /// The instance the client is connected to.
    pub instance: InstanceId,
    /// The instance's current boot epoch.
    pub boot_epoch: BootEpoch,
    /// The protocol version the engine speaks.
    pub protocol: ProtocolVersion,
    /// The application's version, such as `0.1.0`.
    pub version: String,
    /// The newest durable event at the time of the handshake.
    pub head: EventCursor,
    /// Request size limits.
    pub limits: TransportLimits,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_requires_same_major_and_no_newer_minor() {
        let engine = ProtocolVersion { major: 1, minor: 3 };
        assert!(engine.serves(ProtocolVersion { major: 1, minor: 0 }));
        assert!(engine.serves(ProtocolVersion { major: 1, minor: 3 }));
        assert!(!engine.serves(ProtocolVersion { major: 1, minor: 4 }));
        assert!(!engine.serves(ProtocolVersion { major: 2, minor: 0 }));
        assert!(!engine.serves(ProtocolVersion { major: 0, minor: 3 }));
    }

    #[test]
    fn before_1_0_only_the_same_version_is_served() {
        let engine = ProtocolVersion { major: 0, minor: 2 };
        assert!(engine.serves(ProtocolVersion { major: 0, minor: 2 }));
        assert!(!engine.serves(ProtocolVersion { major: 0, minor: 1 }));
        assert!(!engine.serves(ProtocolVersion { major: 0, minor: 3 }));
        assert!(!engine.serves(ProtocolVersion { major: 1, minor: 2 }));
    }

    #[test]
    fn instance_ids_are_unique_and_round_trip() {
        let a = InstanceId::generate();
        assert_ne!(a, InstanceId::generate());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<InstanceId>(&json).unwrap(), a);
    }
}
