//! Typed failures: a stable code, the scope it applies to, and what to do
//! next (ADR-12).
//!
//! Each code accepts only the scopes that make sense for it, so, for
//! example, an invalid credential cannot be recorded against a single file.

use crate::id::{AccountId, ArchiveSetId, AttemptId, JobId, ResourceId, RootId};
use crate::name::{DiagnosticText, HostName, InvalidName, Namespace, ProviderId};
use crate::time::UtcTimestamp;
use crate::version::Generation;

/// Stable failure codes. The snake_case text form is part of the storage and
/// protocol contract; never rename a variant's text.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::Display,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub enum FailureCode {
    /// Connection or response timed out on a replay-safe request.
    RequestTimeout,
    /// A signed or expiring transfer URL is no longer valid.
    LeaseExpired,
    /// An expiring transfer URL returned not-found; the resource itself is
    /// not known to be gone.
    LeaseNotFound,
    /// The outcome of a consumptive provider operation is unknown.
    OperationAmbiguous,
    /// An access token expired and a refresh is available.
    TokenExpired,
    /// The provider rejected the account's credentials.
    InvalidCredentials,
    /// The account's quota is exhausted.
    QuotaExhausted,
    /// The provider cannot currently serve this original host.
    HostUnavailableViaProvider,
    /// The provider confirmed the resource is deleted.
    ResourceDeleted,
    /// A CAPTCHA, password, or other user answer is required.
    ChallengeRequired,
    /// The adapter could not parse the provider's response.
    AdapterParseFailure,
    /// The destination is full, unmounted, or otherwise unavailable.
    DestinationUnavailable,
    /// The destination refused access.
    PermissionDenied,
    /// Bytes did not match the expected hash or representation.
    IntegrityMismatch,
    /// An archive volume is missing from the set.
    MissingArchiveVolume,
    /// A codec or its resource needs are not supported.
    UnsupportedCapability,
    /// The user cancelled the job.
    UserCancelled,
}

/// The kind of [`FailureScope`], used to validate code/scope pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// See [`FailureScope::Attempt`].
    Attempt,
    /// See [`FailureScope::Lease`].
    Lease,
    /// See [`FailureScope::ProviderOperation`].
    ProviderOperation,
    /// See [`FailureScope::Account`].
    Account,
    /// See [`FailureScope::AccountHost`].
    AccountHost,
    /// See [`FailureScope::ProviderHost`].
    ProviderHost,
    /// See [`FailureScope::Resource`].
    Resource,
    /// See [`FailureScope::Adapter`].
    Adapter,
    /// See [`FailureScope::Destination`].
    Destination,
    /// See [`FailureScope::Representation`].
    Representation,
    /// See [`FailureScope::ArchiveSet`].
    ArchiveSet,
    /// See [`FailureScope::Capability`].
    Capability,
    /// See [`FailureScope::JobGeneration`].
    JobGeneration,
}

/// What a failure applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FailureScope {
    /// One attempt, or a request within it.
    Attempt(AttemptId),
    /// The transfer lease issued to an attempt.
    Lease(AttemptId),
    /// The durable provider operation started by an attempt.
    ProviderOperation(AttemptId),
    /// An account, at whatever revision is current.
    Account(AccountId),
    /// One account's access to one original host.
    AccountHost {
        /// The account.
        account: AccountId,
        /// The original host.
        host: HostName,
    },
    /// One provider serving one original host, optionally for one account.
    ProviderHost {
        /// The provider.
        provider: ProviderId,
        /// The account, if the outage is account-specific.
        account: Option<AccountId>,
        /// The original host.
        host: HostName,
    },
    /// A resource identity.
    Resource(ResourceId),
    /// An adapter namespace.
    Adapter(Namespace),
    /// A destination root.
    Destination(RootId),
    /// The partial owned by one job generation.
    Representation {
        /// The job.
        job: JobId,
        /// The generation owning the partial.
        generation: Generation,
    },
    /// An archive set.
    ArchiveSet(ArchiveSetId),
    /// A codec or other capability, by namespace.
    Capability(Namespace),
    /// One job generation.
    JobGeneration {
        /// The job.
        job: JobId,
        /// The generation.
        generation: Generation,
    },
}

impl FailureScope {
    /// This scope's kind.
    pub const fn kind(&self) -> ScopeKind {
        match self {
            Self::Attempt(_) => ScopeKind::Attempt,
            Self::Lease(_) => ScopeKind::Lease,
            Self::ProviderOperation(_) => ScopeKind::ProviderOperation,
            Self::Account(_) => ScopeKind::Account,
            Self::AccountHost { .. } => ScopeKind::AccountHost,
            Self::ProviderHost { .. } => ScopeKind::ProviderHost,
            Self::Resource(_) => ScopeKind::Resource,
            Self::Adapter(_) => ScopeKind::Adapter,
            Self::Destination(_) => ScopeKind::Destination,
            Self::Representation { .. } => ScopeKind::Representation,
            Self::ArchiveSet(_) => ScopeKind::ArchiveSet,
            Self::Capability(_) => ScopeKind::Capability,
            Self::JobGeneration { .. } => ScopeKind::JobGeneration,
        }
    }
}

/// What the engine does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NextAction {
    /// Retry within the retry budget, with backoff, after revalidating any
    /// partial state.
    RetryWithBackoff,
    /// Resolve again and validate the representation before reusing bytes.
    Reresolve,
    /// Reconcile through the provider's operation ID or idempotency key;
    /// otherwise surface the ambiguity. Never blindly repeat.
    Reconcile,
    /// Refresh the account once (single flight), then retry safe work.
    RefreshAccount,
    /// Wait until the scope is eligible again.
    WaitUntilEligible,
    /// Stop automatic attempts and ask the user.
    UserAction,
    /// Open the adapter's circuit breaker; no hot retry loop.
    CircuitBreak,
    /// Block affected work, keep data, and wait for remediation.
    BlockUntilRemediated,
    /// Quarantine or restart the owned partial; never publish it.
    RestartPartial,
    /// Wait for the identified missing part.
    WaitForPart,
    /// Terminal: do not retry automatically.
    Terminal,
    /// Stop work and reject late results.
    Stop,
}

impl FailureCode {
    /// The stable text code.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// The scopes this code may be recorded against (failure matrix, 7.4).
    pub const fn allowed_scopes(self) -> &'static [ScopeKind] {
        use ScopeKind as S;
        match self {
            Self::RequestTimeout => &[S::Attempt],
            Self::LeaseExpired | Self::LeaseNotFound => &[S::Lease],
            Self::OperationAmbiguous => &[S::ProviderOperation],
            Self::TokenExpired | Self::InvalidCredentials => &[S::Account],
            Self::QuotaExhausted => &[S::Account, S::AccountHost],
            Self::HostUnavailableViaProvider => &[S::ProviderHost],
            Self::ResourceDeleted => &[S::Resource],
            Self::ChallengeRequired => &[S::Attempt, S::JobGeneration],
            Self::AdapterParseFailure => &[S::Adapter],
            Self::DestinationUnavailable | Self::PermissionDenied => &[S::Destination],
            Self::IntegrityMismatch => &[S::Representation],
            Self::MissingArchiveVolume => &[S::ArchiveSet],
            Self::UnsupportedCapability => &[S::Capability],
            Self::UserCancelled => &[S::JobGeneration],
        }
    }

    /// The default next action (failure matrix, 7.4).
    pub const fn next_action(self) -> NextAction {
        match self {
            Self::RequestTimeout => NextAction::RetryWithBackoff,
            Self::LeaseExpired => NextAction::Reresolve,
            // Investigate by re-resolving; never conclude the resource is gone.
            Self::LeaseNotFound => NextAction::Reresolve,
            Self::OperationAmbiguous => NextAction::Reconcile,
            Self::TokenExpired => NextAction::RefreshAccount,
            Self::InvalidCredentials | Self::ChallengeRequired => NextAction::UserAction,
            Self::QuotaExhausted | Self::HostUnavailableViaProvider => {
                NextAction::WaitUntilEligible
            }
            Self::ResourceDeleted => NextAction::Terminal,
            Self::AdapterParseFailure => NextAction::CircuitBreak,
            Self::DestinationUnavailable | Self::PermissionDenied => {
                NextAction::BlockUntilRemediated
            }
            Self::IntegrityMismatch => NextAction::RestartPartial,
            Self::MissingArchiveVolume => NextAction::WaitForPart,
            Self::UnsupportedCapability => NextAction::BlockUntilRemediated,
            Self::UserCancelled => NextAction::Stop,
        }
    }

    /// Precedence when one attempt observes several failures; the highest
    /// value decides the outcome. This is an explicit table, independent of
    /// declaration order (ADR-12). User intent first, then data safety, then
    /// failures needing a person, then the widest scopes, then transient
    /// conditions.
    pub const fn priority(self) -> u8 {
        match self {
            Self::UserCancelled => 100,
            Self::IntegrityMismatch => 90,
            Self::PermissionDenied => 85,
            Self::DestinationUnavailable => 80,
            Self::OperationAmbiguous => 75,
            Self::InvalidCredentials => 70,
            Self::ChallengeRequired => 65,
            Self::ResourceDeleted => 60,
            Self::UnsupportedCapability => 55,
            Self::MissingArchiveVolume => 50,
            Self::QuotaExhausted => 45,
            Self::HostUnavailableViaProvider => 40,
            Self::AdapterParseFailure => 35,
            Self::TokenExpired => 30,
            Self::LeaseNotFound => 25,
            Self::LeaseExpired => 20,
            Self::RequestTimeout => 10,
        }
    }
}

/// A provider's own error code, kept under the adapter's namespace so it is
/// never confused with a [`FailureCode`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderCode {
    namespace: Namespace,
    code: String,
}

impl ProviderCode {
    /// Maximum code length in bytes.
    pub const MAX_LEN: usize = 64;

    /// Validates a provider code: 1 to 64 printable ASCII bytes.
    pub fn new(namespace: Namespace, code: &str) -> Result<Self, InvalidName> {
        if code.is_empty()
            || code.len() > Self::MAX_LEN
            || !code.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(InvalidName::new(
                "provider code",
                "must be 1 to 64 printable ASCII bytes",
            ));
        }
        Ok(Self {
            namespace,
            code: code.to_owned(),
        })
    }

    /// The owning adapter namespace.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// The provider's code.
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// Bounded automatic retries for one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBudget {
    used: u32,
    limit: u32,
}

impl RetryBudget {
    /// A fresh budget allowing `limit` retries.
    pub const fn new(limit: u32) -> Self {
        Self { used: 0, limit }
    }

    /// Restores a stored budget; `used` is clamped to `limit`.
    pub const fn restore(used: u32, limit: u32) -> Self {
        Self {
            used: if used < limit { used } else { limit },
            limit,
        }
    }

    /// Retries consumed so far.
    pub const fn used(self) -> u32 {
        self.used
    }

    /// Retry limit.
    pub const fn limit(self) -> u32 {
        self.limit
    }

    /// Whether no retries remain.
    pub const fn is_exhausted(self) -> bool {
        self.used >= self.limit
    }

    /// Consumes one retry, or returns `None` if the budget is exhausted.
    pub const fn consume(self) -> Option<Self> {
        if self.is_exhausted() {
            None
        } else {
            Some(Self {
                used: self.used + 1,
                limit: self.limit,
            })
        }
    }
}

/// A code was recorded against a scope the failure matrix does not allow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code} cannot apply to a {scope:?} scope")]
pub struct ScopeMismatch {
    /// The code.
    pub code: FailureCode,
    /// The rejected scope kind.
    pub scope: ScopeKind,
}

/// A failure as persisted and reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    code: FailureCode,
    scope: FailureScope,
    earliest_retry: Option<UtcTimestamp>,
    provider_code: Option<ProviderCode>,
    context: Option<DiagnosticText>,
}

impl Failure {
    /// Builds a failure, rejecting scopes the code does not allow.
    pub fn new(code: FailureCode, scope: FailureScope) -> Result<Self, ScopeMismatch> {
        let kind = scope.kind();
        if !code.allowed_scopes().contains(&kind) {
            return Err(ScopeMismatch { code, scope: kind });
        }
        Ok(Self {
            code,
            scope,
            earliest_retry: None,
            provider_code: None,
            context: None,
        })
    }

    /// Adds a minimum wait. Waits only ever lengthen: a later constraint
    /// such as a valid `Retry-After` cannot be shortened by an earlier one.
    pub fn with_earliest_retry(mut self, at: UtcTimestamp) -> Self {
        self.earliest_retry = Some(self.earliest_retry.map_or(at, |t| t.max(at)));
        self
    }

    /// Attaches the provider's own code.
    pub fn with_provider_code(mut self, code: ProviderCode) -> Self {
        self.provider_code = Some(code);
        self
    }

    /// Attaches redacted diagnostic context.
    pub fn with_context(mut self, context: DiagnosticText) -> Self {
        self.context = Some(context);
        self
    }

    /// The stable code.
    pub const fn code(&self) -> FailureCode {
        self.code
    }

    /// The affected scope.
    pub const fn scope(&self) -> &FailureScope {
        &self.scope
    }

    /// The next action implied by the code.
    pub const fn next_action(&self) -> NextAction {
        self.code.next_action()
    }

    /// The earliest time the scope may be retried, if constrained.
    pub const fn earliest_retry(&self) -> Option<UtcTimestamp> {
        self.earliest_retry
    }

    /// The provider's own code, if any.
    pub const fn provider_code(&self) -> Option<&ProviderCode> {
        self.provider_code.as_ref()
    }

    /// Diagnostic context, if any.
    pub const fn context(&self) -> Option<&DiagnosticText> {
        self.context.as_ref()
    }

    /// The failure that decides an attempt's outcome: highest priority,
    /// first reported on a tie.
    pub fn decisive<'a>(failures: impl IntoIterator<Item = &'a Failure>) -> Option<&'a Failure> {
        failures
            .into_iter()
            .fold(None, |best: Option<&Failure>, f| match best {
                Some(b) if b.code.priority() >= f.code.priority() => Some(b),
                _ => Some(f),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use strum::VariantArray;

    fn account() -> AccountId {
        AccountId::from_u64(1).unwrap()
    }

    fn host() -> HostName {
        HostName::new("files.example").unwrap()
    }

    fn attempt() -> AttemptId {
        AttemptId::from_u64(1).unwrap()
    }

    #[test]
    fn codes_round_trip_and_are_unique() {
        let mut seen = HashSet::new();
        for &code in FailureCode::VARIANTS {
            assert!(seen.insert(code.as_str()), "duplicate {code}");
            assert_eq!(code.as_str().parse(), Ok(code));
        }
        assert!("unknown".parse::<FailureCode>().is_err());
    }

    #[test]
    fn every_code_has_a_scope_and_a_distinct_priority() {
        let mut priorities = HashSet::new();
        for &code in FailureCode::VARIANTS {
            assert!(!code.allowed_scopes().is_empty(), "{code}");
            assert!(
                priorities.insert(code.priority()),
                "tied priority for {code}"
            );
        }
    }

    #[test]
    fn priority_is_not_declaration_order() {
        // UserCancelled is declared last but outranks everything; the first
        // declared code ranks lowest.
        let max = FailureCode::VARIANTS
            .iter()
            .max_by_key(|c| c.priority())
            .unwrap();
        let min = FailureCode::VARIANTS
            .iter()
            .min_by_key(|c| c.priority())
            .unwrap();
        assert_eq!(*max, FailureCode::UserCancelled);
        assert_eq!(*min, FailureCode::RequestTimeout);
        assert!(FailureCode::IntegrityMismatch.priority() > FailureCode::RequestTimeout.priority());
    }

    #[test]
    fn credential_failures_cannot_be_scoped_to_a_file() {
        let res = FailureScope::Resource(ResourceId::from_u64(1).unwrap());
        assert_eq!(
            Failure::new(FailureCode::InvalidCredentials, res.clone()),
            Err(ScopeMismatch {
                code: FailureCode::InvalidCredentials,
                scope: ScopeKind::Resource
            }),
        );
        assert!(
            Failure::new(
                FailureCode::InvalidCredentials,
                FailureScope::Account(account())
            )
            .is_ok()
        );
    }

    #[test]
    fn provider_outage_is_not_resource_deletion() {
        let tuple = FailureScope::ProviderHost {
            provider: ProviderId::new("debrid-example").unwrap(),
            account: Some(account()),
            host: host(),
        };
        let res = FailureScope::Resource(ResourceId::from_u64(1).unwrap());
        assert!(Failure::new(FailureCode::HostUnavailableViaProvider, tuple.clone()).is_ok());
        assert!(Failure::new(FailureCode::HostUnavailableViaProvider, res.clone()).is_err());
        assert!(Failure::new(FailureCode::ResourceDeleted, tuple).is_err());
        // A 404 on an expiring URL is scoped to the lease and re-resolves.
        assert!(Failure::new(FailureCode::LeaseNotFound, res).is_err());
        let f = Failure::new(FailureCode::LeaseNotFound, FailureScope::Lease(attempt())).unwrap();
        assert_eq!(f.next_action(), NextAction::Reresolve);
    }

    #[test]
    fn quota_scope_may_be_account_or_account_host() {
        let pair = FailureScope::AccountHost {
            account: account(),
            host: host(),
        };
        assert!(Failure::new(FailureCode::QuotaExhausted, pair).is_ok());
        assert!(
            Failure::new(
                FailureCode::QuotaExhausted,
                FailureScope::Account(account())
            )
            .is_ok()
        );
        assert!(
            Failure::new(
                FailureCode::QuotaExhausted,
                FailureScope::Attempt(attempt())
            )
            .is_err()
        );
    }

    #[test]
    fn minimum_wait_never_shortens() {
        let f = Failure::new(
            FailureCode::QuotaExhausted,
            FailureScope::Account(account()),
        )
        .unwrap()
        .with_earliest_retry(UtcTimestamp::from_unix_millis(5_000))
        .with_earliest_retry(UtcTimestamp::from_unix_millis(1_000));
        assert_eq!(
            f.earliest_retry(),
            Some(UtcTimestamp::from_unix_millis(5_000))
        );
    }

    #[test]
    fn decisive_failure_uses_priority_then_report_order() {
        let timeout = Failure::new(
            FailureCode::RequestTimeout,
            FailureScope::Attempt(attempt()),
        )
        .unwrap();
        let disk = Failure::new(
            FailureCode::DestinationUnavailable,
            FailureScope::Destination(RootId::from_u64(1).unwrap()),
        )
        .unwrap();
        let disk2 = disk.clone().with_context(DiagnosticText::new("second"));
        let all = [timeout.clone(), disk.clone(), disk2];
        assert_eq!(Failure::decisive(&all), Some(&disk));
        assert_eq!(
            Failure::decisive(std::slice::from_ref(&timeout)),
            Some(&timeout)
        );
        assert_eq!(Failure::decisive(&[]), None);
    }

    #[test]
    fn retry_budget_is_bounded() {
        let budget = RetryBudget::new(2);
        let budget = budget.consume().unwrap().consume().unwrap();
        assert!(budget.is_exhausted());
        assert_eq!(budget.consume(), None);
        assert_eq!(RetryBudget::restore(9, 3).used(), 3);
    }

    #[test]
    fn provider_codes_are_bounded_and_namespaced() {
        let ns = Namespace::new("example-host").unwrap();
        let code = ProviderCode::new(ns.clone(), "E_LIMIT").unwrap();
        assert_eq!(code.namespace(), &ns);
        assert!(ProviderCode::new(ns.clone(), "").is_err());
        assert!(ProviderCode::new(ns.clone(), "has space").is_err());
        assert!(ProviderCode::new(ns, &"x".repeat(65)).is_err());
    }
}
