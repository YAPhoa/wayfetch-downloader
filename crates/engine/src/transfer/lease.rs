//! Transfer offers, one-use leases, and request replay rules (section 6.5,
//! ADR-09; tests T11 and T12).

use std::num::NonZeroU16;

use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::id::{AttemptId, JobId};
use wayfetch_domain::source::Candidate;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;

/// The broker's handle to a prepared request: method, URL, headers, and
/// body stay with the broker, so secrets never travel with the lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestRef(pub u64);

/// The broker's handle to a response it already opened, for example the
/// download that a form submission returned directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResponseRef(pub u64);

/// Where the transfer's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseSource {
    /// A request the broker can send.
    PreparedRequest(RequestRef),
    /// A response that is already open. The broker hands its body out once;
    /// it never licenses repeating the request that produced it.
    OpenResponse(ResponseRef),
}

/// Whether the request behind an offer may be sent again without asking
/// the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPolicy {
    /// Repeating it has no side effect (a plain GET of a stable URL).
    Idempotent,
    /// The adapter decides each repetition, for example because the
    /// provider counts downloads.
    AdapterControlled,
    /// Never repeated automatically (a consumptive POST, a one-use link).
    /// A lost result is reconciled or shown as ambiguous, never resent.
    NeverAutomaticallyReplay,
}

/// Whether an interrupted transfer may continue from its durable bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumePolicy {
    /// Always restart from zero.
    Forbidden,
    /// Continue with a range request conditioned on the representation's
    /// validators.
    Conditional,
    /// As `Conditional`, and disjoint segments may be fetched in parallel.
    VerifiedSegmented,
}

/// How many connections the offer allows at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parallelism {
    /// One connection.
    Sequential,
    /// Up to this many connections, each for its own interval.
    UpTo(NonZeroU16),
}

impl Parallelism {
    /// The connection limit.
    pub const fn limit(self) -> u16 {
        match self {
            Self::Sequential => 1,
            Self::UpTo(n) => n.get(),
        }
    }
}

/// What identifies the bytes of one representation, as reported by the
/// server it came from. Two values are equivalent only within one offer's
/// candidate (provider, account, route): a matching ETag from another CDN
/// does not make the bytes interchangeable (rule 7.5.5, T06).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Representation {
    /// Complete length in bytes, if known.
    pub length: Option<u64>,
    /// Strong entity tag, if the server sent one. Weak tags are not kept:
    /// they cannot make a range request conditional.
    pub strong_etag: Option<String>,
    /// `Last-Modified`, if sent.
    pub last_modified: Option<UtcTimestamp>,
}

impl Representation {
    /// Whether the identity is strong enough to combine bytes from more
    /// than one response: a strong ETag, or a modification time with a
    /// known length. Without it a transfer restarts rather than mixing old
    /// and new bytes (rule 7.5.4).
    pub fn is_trustworthy(&self) -> bool {
        self.strong_etag.is_some() || (self.last_modified.is_some() && self.length.is_some())
    }
}

/// What an adapter's resolve step hands the engine (section 6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOffer {
    /// The resource, provider, account, and route the offer is bound to.
    pub candidate: Candidate,
    /// What the adapter already knows about the representation.
    pub representation: Representation,
    /// The request or response to read.
    pub source: LeaseSource,
    /// When the offer stops being usable, for example a signed URL's
    /// expiry.
    pub expires_at: Option<UtcTimestamp>,
    /// Whether the request may be repeated.
    pub replay: ReplayPolicy,
    /// Whether a transfer may continue from durable bytes.
    pub resume: ResumePolicy,
    /// Connection limit.
    pub parallelism: Parallelism,
}

impl TransferOffer {
    fn expired(&self, now: UtcTimestamp) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

/// Why a lease cannot start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// The offer expired before use.
    #[error("transfer lease expired")]
    Expired,
    /// The job moved to another generation after the lease was issued.
    #[error("transfer lease is for generation {presented}, the job is at {current}")]
    StaleGeneration {
        /// The job's generation.
        current: Generation,
        /// The lease's generation.
        presented: Generation,
    },
}

impl LeaseError {
    /// The failure to record, if any. A stale lease belongs to abandoned
    /// work and records nothing.
    pub fn failure_code(self) -> Option<FailureCode> {
        match self {
            Self::Expired => Some(FailureCode::LeaseExpired),
            Self::StaleGeneration { .. } => None,
        }
    }
}

/// An offer issued to one attempt of one job generation. Starting a
/// transfer consumes it, so it is used at most once (section 6.5). That is
/// a local guarantee: it says nothing about how often a remote provider
/// saw the request.
#[derive(Debug)]
pub struct TransferLease {
    offer: TransferOffer,
    job: JobId,
    generation: Generation,
    attempt: AttemptId,
}

impl TransferLease {
    /// Issues `offer` to `attempt` of `job` at `generation`.
    pub fn new(
        offer: TransferOffer,
        job: JobId,
        generation: Generation,
        attempt: AttemptId,
    ) -> Self {
        Self {
            offer,
            job,
            generation,
            attempt,
        }
    }

    /// The offer.
    pub fn offer(&self) -> &TransferOffer {
        &self.offer
    }

    /// Starts the transfer, consuming the lease, if it has not expired and
    /// the job is still at the lease's generation.
    pub fn start(self, current: Generation, now: UtcTimestamp) -> Result<Transfer, LeaseError> {
        if current != self.generation {
            return Err(LeaseError::StaleGeneration {
                current,
                presented: self.generation,
            });
        }
        if self.offer.expired(now) {
            return Err(LeaseError::Expired);
        }
        Ok(Transfer {
            lease: self,
            observed: None,
        })
    }
}

/// Why another request is wanted for a started transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPurpose {
    /// Send the request again after it failed before any body byte was
    /// written, for example a connect timeout.
    Retry,
    /// Continue after the durable bytes with a range request.
    Resume,
    /// Open one more connection for a disjoint interval, with `open`
    /// connections already running.
    Segment {
        /// Connections already open.
        open: u16,
    },
}

/// The answer to a [`RequestPurpose`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replay {
    /// Send it.
    Allowed,
    /// Do not continue the partial; a full request from zero in a new
    /// generation is allowed.
    RestartFromZero,
    /// The adapter must decide.
    AskAdapter,
    /// This offer cannot serve it; resolve a new one. For a consumptive
    /// request that may mean reconciling an ambiguous outcome first (T12).
    Reresolve,
    /// Not allowed; keep the current connections.
    Denied,
}

/// A started transfer: the consumed lease plus the representation its
/// first response showed.
#[derive(Debug)]
pub struct Transfer {
    lease: TransferLease,
    observed: Option<Representation>,
}

impl Transfer {
    /// The offer.
    pub fn offer(&self) -> &TransferOffer {
        &self.lease.offer
    }

    /// The job.
    pub fn job(&self) -> JobId {
        self.lease.job
    }

    /// The job generation the transfer belongs to.
    pub fn generation(&self) -> Generation {
        self.lease.generation
    }

    /// The attempt.
    pub fn attempt(&self) -> AttemptId {
        self.lease.attempt
    }

    /// Applies the replay and resume rules to another request (section
    /// 6.5). An open response is never cloned; a request is never repeated
    /// against an expired offer; bytes from several responses are combined
    /// only under a trustworthy, unchanged representation.
    pub fn authorize(&self, purpose: RequestPurpose, now: UtcTimestamp) -> Replay {
        let offer = self.offer();
        if offer.expired(now) || matches!(offer.source, LeaseSource::OpenResponse(_)) {
            return Replay::Reresolve;
        }
        match offer.replay {
            ReplayPolicy::NeverAutomaticallyReplay => return Replay::Reresolve,
            ReplayPolicy::AdapterControlled => return Replay::AskAdapter,
            ReplayPolicy::Idempotent => {}
        }
        let trusted = self
            .observed
            .as_ref()
            .is_some_and(Representation::is_trustworthy);
        match purpose {
            RequestPurpose::Retry => Replay::Allowed,
            RequestPurpose::Resume => match offer.resume {
                ResumePolicy::Conditional | ResumePolicy::VerifiedSegmented if trusted => {
                    Replay::Allowed
                }
                _ => Replay::RestartFromZero,
            },
            RequestPurpose::Segment { open } => {
                if offer.resume == ResumePolicy::VerifiedSegmented
                    && trusted
                    && open < offer.parallelism.limit()
                {
                    Replay::Allowed
                } else {
                    Replay::Denied
                }
            }
        }
    }

    /// Records the representation a response reported. The first response
    /// establishes it, checked against what the offer promised; every later
    /// one must match exactly, or its bytes must not join the partial
    /// (rules 7.5.4 and 7.5.5, T05).
    pub fn observe(&mut self, seen: Representation) -> Result<(), RepresentationChanged> {
        match &self.observed {
            Some(established) if *established != seen => Err(RepresentationChanged),
            Some(_) => Ok(()),
            None => {
                let promised = &self.lease.offer.representation;
                if conflicts(&promised.length, &seen.length)
                    || conflicts(&promised.strong_etag, &seen.strong_etag)
                    || conflicts(&promised.last_modified, &seen.last_modified)
                {
                    return Err(RepresentationChanged);
                }
                self.observed = Some(seen);
                Ok(())
            }
        }
    }

    /// The representation established by the first response, if any.
    pub fn representation(&self) -> Option<&Representation> {
        self.observed.as_ref()
    }
}

/// Whether two optional values are both known and differ.
fn conflicts<T: PartialEq>(a: &Option<T>, b: &Option<T>) -> bool {
    a.is_some() && b.is_some() && a != b
}

/// A response showed a different representation than the transfer
/// established; its bytes must not be combined with the partial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the representation changed during the transfer")]
pub struct RepresentationChanged;

impl RepresentationChanged {
    /// The failure to record: restart the partial.
    pub const fn failure_code(self) -> FailureCode {
        FailureCode::IntegrityMismatch
    }
}

#[cfg(test)]
mod tests {
    use wayfetch_domain::id::{ResourceId, RouteId};
    use wayfetch_domain::name::{HostName, ProviderId};

    use super::*;

    const NOW: UtcTimestamp = UtcTimestamp::from_unix_millis(1_000_000);

    fn offer(source: LeaseSource, replay: ReplayPolicy, resume: ResumePolicy) -> TransferOffer {
        TransferOffer {
            candidate: Candidate {
                resource: ResourceId::from_u64(1).unwrap(),
                original_host: HostName::new("files.example").unwrap(),
                provider: ProviderId::new("direct").unwrap(),
                account: None,
                route: RouteId::from_u64(1).unwrap(),
            },
            representation: Representation::default(),
            source,
            expires_at: None,
            replay,
            resume,
            parallelism: Parallelism::UpTo(NonZeroU16::new(2).unwrap()),
        }
    }

    fn start(offer: TransferOffer) -> Transfer {
        TransferLease::new(
            offer,
            JobId::from_u64(1).unwrap(),
            Generation::INITIAL,
            AttemptId::from_u64(1).unwrap(),
        )
        .start(Generation::INITIAL, NOW)
        .unwrap()
    }

    fn trusted() -> Representation {
        Representation {
            length: Some(100),
            strong_etag: Some("\"v1\"".into()),
            last_modified: None,
        }
    }

    const GET: LeaseSource = LeaseSource::PreparedRequest(RequestRef(1));

    #[test]
    fn a_lease_starts_once_and_only_for_its_generation() {
        let lease = |expires_at| {
            let mut o = offer(GET, ReplayPolicy::Idempotent, ResumePolicy::Conditional);
            o.expires_at = expires_at;
            TransferLease::new(
                o,
                JobId::from_u64(1).unwrap(),
                Generation::INITIAL,
                AttemptId::from_u64(1).unwrap(),
            )
        };
        let next = Generation::INITIAL.next().unwrap();
        assert!(matches!(
            lease(None).start(next, NOW),
            Err(LeaseError::StaleGeneration { .. })
        ));
        assert_eq!(
            lease(Some(NOW))
                .start(Generation::INITIAL, NOW)
                .unwrap_err(),
            LeaseError::Expired
        );
        assert_eq!(
            LeaseError::Expired.failure_code(),
            Some(FailureCode::LeaseExpired)
        );
        // `start` takes the lease by value; a second start does not compile.
        assert!(lease(None).start(Generation::INITIAL, NOW).is_ok());
    }

    #[test]
    fn open_responses_and_one_use_requests_are_never_replayed() {
        let open = start(offer(
            LeaseSource::OpenResponse(ResponseRef(9)),
            ReplayPolicy::Idempotent,
            ResumePolicy::VerifiedSegmented,
        ));
        let post = start(offer(
            GET,
            ReplayPolicy::NeverAutomaticallyReplay,
            ResumePolicy::Conditional,
        ));
        let counted = start(offer(
            GET,
            ReplayPolicy::AdapterControlled,
            ResumePolicy::Conditional,
        ));
        for purpose in [
            RequestPurpose::Retry,
            RequestPurpose::Resume,
            RequestPurpose::Segment { open: 1 },
        ] {
            assert_eq!(open.authorize(purpose, NOW), Replay::Reresolve);
            assert_eq!(post.authorize(purpose, NOW), Replay::Reresolve);
            assert_eq!(counted.authorize(purpose, NOW), Replay::AskAdapter);
        }
    }

    #[test]
    fn resume_and_segments_need_a_trusted_identity() {
        let mut t = start(offer(
            GET,
            ReplayPolicy::Idempotent,
            ResumePolicy::VerifiedSegmented,
        ));
        assert_eq!(t.authorize(RequestPurpose::Retry, NOW), Replay::Allowed);
        assert_eq!(
            t.authorize(RequestPurpose::Resume, NOW),
            Replay::RestartFromZero
        );
        assert_eq!(
            t.authorize(RequestPurpose::Segment { open: 1 }, NOW),
            Replay::Denied
        );

        t.observe(trusted()).unwrap();
        assert_eq!(t.authorize(RequestPurpose::Resume, NOW), Replay::Allowed);
        assert_eq!(
            t.authorize(RequestPurpose::Segment { open: 1 }, NOW),
            Replay::Allowed
        );
        assert_eq!(
            t.authorize(RequestPurpose::Segment { open: 2 }, NOW),
            Replay::Denied
        );

        let mut conditional = start(offer(
            GET,
            ReplayPolicy::Idempotent,
            ResumePolicy::Conditional,
        ));
        conditional.observe(trusted()).unwrap();
        assert_eq!(
            conditional.authorize(RequestPurpose::Segment { open: 1 }, NOW),
            Replay::Denied
        );
        let mut forbidden = start(offer(
            GET,
            ReplayPolicy::Idempotent,
            ResumePolicy::Forbidden,
        ));
        forbidden.observe(trusted()).unwrap();
        assert_eq!(
            forbidden.authorize(RequestPurpose::Resume, NOW),
            Replay::RestartFromZero
        );
    }

    #[test]
    fn an_expired_offer_cannot_send_more_requests() {
        let mut o = offer(GET, ReplayPolicy::Idempotent, ResumePolicy::Conditional);
        o.expires_at = Some(UtcTimestamp::from_unix_millis(NOW.unix_millis() + 10));
        let t = start(o);
        let later = UtcTimestamp::from_unix_millis(NOW.unix_millis() + 10);
        assert_eq!(t.authorize(RequestPurpose::Retry, later), Replay::Reresolve);
    }

    #[test]
    fn the_representation_cannot_change_mid_transfer() {
        let mut promised = offer(GET, ReplayPolicy::Idempotent, ResumePolicy::Conditional);
        promised.representation.length = Some(50);
        assert_eq!(
            start(promised).observe(trusted()),
            Err(RepresentationChanged)
        );

        let mut t = start(offer(
            GET,
            ReplayPolicy::Idempotent,
            ResumePolicy::Conditional,
        ));
        t.observe(trusted()).unwrap();
        t.observe(trusted()).unwrap();
        let mut changed = trusted();
        changed.strong_etag = Some("\"v2\"".into());
        assert_eq!(t.observe(changed), Err(RepresentationChanged));
        let mut dropped = trusted();
        dropped.strong_etag = None;
        assert_eq!(t.observe(dropped), Err(RepresentationChanged));
        assert_eq!(t.representation(), Some(&trusted()));
    }
}
