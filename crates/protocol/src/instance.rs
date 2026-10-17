//! Engine instance identity and the client handshake (architecture brief
//! sections 4.7 and 11.2).
//!
//! Before any mutation, a client learns which engine instance it is talking
//! to, that instance's current boot epoch, and whether its protocol version
//! is compatible. Commands name their target instance, so a client that
//! reconnects to a different instance (another state directory, or a remote
//! host that changed) is refused instead of acting on the wrong queue.

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
/// A minor version may only add optional fields, which older decoders
/// ignore, and new commands or queries, which older clients never send. Any
/// new value a client must decode from the engine (an event kind, stream
/// item, error code, job stage, failure code, or resync reason) needs a new
/// major version, because an older client cannot decode it. Extending those
/// vocabularies within a major version needs capability negotiation first.
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
    pub const CURRENT: Self = Self { major: 2, minor: 3 };

    /// Whether an engine speaking `self` can serve a client speaking
    /// `client`: same major version, and the client needs nothing newer than
    /// the engine provides.
    pub const fn serves(self, client: Self) -> bool {
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
    /// Maximum links waiting in the collected list. Added in protocol 1.1.
    #[serde(default)]
    pub max_collected_links: u32,
}

impl TransportLimits {
    /// Limits of this protocol version.
    pub const DEFAULT: Self = Self {
        max_page_size: crate::query::PageSize::MAX.get(),
        max_submit_items: crate::command::MAX_SUBMIT_ITEMS as u16,
        max_url_bytes: crate::command::MAX_URL_BYTES as u32,
        max_collected_links: crate::command::MAX_COLLECTED_LINKS as u32,
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
    fn instance_ids_are_unique_and_round_trip() {
        let a = InstanceId::generate();
        assert_ne!(a, InstanceId::generate());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<InstanceId>(&json).unwrap(), a);
    }
}
