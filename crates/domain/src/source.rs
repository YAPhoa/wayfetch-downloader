//! Source identity and the scheduled-download tuple.
//!
//! A resource is identified by its adapter namespace, canonical key, variant,
//! and visibility scope, never by a raw URL or filename (ADR-10). A download
//! candidate additionally names the provider, account, and egress route that
//! will serve it, so quotas, credentials, and cooldowns never collapse into a
//! single `host` field.

use crate::id::{AccountId, ResourceId, RouteId};
use crate::name::{CanonicalKey, HostName, Namespace, ProviderId, Variant};

/// Who may treat a resource as the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisibilityScope {
    /// The resource means the same thing to every account.
    Public,
    /// The resource's meaning depends on the account that sees it, such as a
    /// private file or an account-specific link.
    Account(AccountId),
}

/// How firmly the identity was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentityConfidence {
    /// Derived from the submitted input only; may be merged or split once an
    /// adapter establishes the canonical identity.
    Provisional,
    /// Established by the adapter that owns the namespace.
    Canonical,
}

/// The stable identity of a resource.
///
/// Two identities are the same resource only if every field is equal. In
/// particular, two variants of one key, or one key seen through two account
/// scopes, are distinct resources. Name and size are deliberately absent:
/// they never establish identity (ADR-10 gate).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResourceIdentity {
    /// Adapter namespace that owns `canonical_key`.
    pub namespace: Namespace,
    /// Adapter-defined canonical key within the namespace.
    pub canonical_key: CanonicalKey,
    /// Variant selection, if the resource has several.
    pub variant: Option<Variant>,
    /// Account scope, if the resource's meaning depends on the account.
    pub visibility: VisibilityScope,
}

/// A stored resource: its identity plus descriptive fields that do not take
/// part in identity comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    /// Store identifier.
    pub id: ResourceId,
    /// Identity used for deduplication.
    pub identity: ResourceIdentity,
    /// Host of the original input. Distinct from the provider that may
    /// resolve it and from the eventual transfer origin.
    pub original_host: HostName,
    /// How firmly `identity` was established.
    pub confidence: IdentityConfidence,
}

impl Resource {
    /// Whether two resources may be deduplicated. Only canonical identities
    /// are merged; a provisional identity is kept separate until an adapter
    /// confirms it.
    pub fn is_same_resource(&self, other: &Self) -> bool {
        self.confidence == IdentityConfidence::Canonical
            && other.confidence == IdentityConfidence::Canonical
            && self.identity == other.identity
    }
}

/// One way to obtain a resource: the original host, the provider that
/// resolves it, the account used, and the egress route. Scheduler admission,
/// cooldowns, and session reuse key on this tuple, not on a single host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// The resource to download.
    pub resource: ResourceId,
    /// Host of the original input.
    pub original_host: HostName,
    /// Resolver or provider handling the download; may differ from the
    /// original host (for example a multihost provider).
    pub provider: ProviderId,
    /// Account used with the provider, if any.
    pub account: Option<AccountId>,
    /// Egress route for the provider's requests.
    pub route: RouteId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(key: &str, variant: Option<&str>, visibility: VisibilityScope) -> ResourceIdentity {
        ResourceIdentity {
            namespace: Namespace::new("example-host").unwrap(),
            canonical_key: CanonicalKey::new(key).unwrap(),
            variant: variant.map(|v| Variant::new(v).unwrap()),
            visibility,
        }
    }

    fn resource(id: u64, identity: ResourceIdentity, confidence: IdentityConfidence) -> Resource {
        Resource {
            id: ResourceId::from_u64(id).unwrap(),
            identity,
            original_host: HostName::new("files.example").unwrap(),
            confidence,
        }
    }

    #[test]
    fn variants_remain_distinct() {
        let hd = resource(
            1,
            identity("v/1", Some("1080p"), VisibilityScope::Public),
            IdentityConfidence::Canonical,
        );
        let sd = resource(
            2,
            identity("v/1", Some("480p"), VisibilityScope::Public),
            IdentityConfidence::Canonical,
        );
        assert!(!hd.is_same_resource(&sd));
    }

    #[test]
    fn account_scopes_remain_distinct() {
        let a = AccountId::from_u64(1).unwrap();
        let b = AccountId::from_u64(2).unwrap();
        let ra = resource(
            1,
            identity("f/1", None, VisibilityScope::Account(a)),
            IdentityConfidence::Canonical,
        );
        let rb = resource(
            2,
            identity("f/1", None, VisibilityScope::Account(b)),
            IdentityConfidence::Canonical,
        );
        let public = resource(
            3,
            identity("f/1", None, VisibilityScope::Public),
            IdentityConfidence::Canonical,
        );
        assert!(!ra.is_same_resource(&rb));
        assert!(!ra.is_same_resource(&public));
    }

    #[test]
    fn provisional_identities_are_not_merged() {
        let id = identity("f/1", None, VisibilityScope::Public);
        let canonical = resource(1, id.clone(), IdentityConfidence::Canonical);
        let provisional = resource(2, id.clone(), IdentityConfidence::Provisional);
        assert!(!canonical.is_same_resource(&provisional));
        assert!(canonical.is_same_resource(&resource(3, id, IdentityConfidence::Canonical)));
    }

    #[test]
    fn candidate_keeps_host_provider_account_and_route_apart() {
        let base = Candidate {
            resource: ResourceId::from_u64(1).unwrap(),
            original_host: HostName::new("files.example").unwrap(),
            provider: ProviderId::new("debrid-example").unwrap(),
            account: AccountId::from_u64(7),
            route: RouteId::from_u64(1).unwrap(),
        };
        let other_route = Candidate {
            route: RouteId::from_u64(2).unwrap(),
            ..base.clone()
        };
        let other_account = Candidate {
            account: AccountId::from_u64(8),
            ..base.clone()
        };
        let direct = Candidate {
            provider: ProviderId::new("files.example").unwrap(),
            account: None,
            ..base.clone()
        };
        assert_ne!(base, other_route);
        assert_ne!(base, other_account);
        assert_ne!(base, direct);
    }
}
