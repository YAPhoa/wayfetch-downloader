//! File-job state: desired state, observed stage, revision, and generation.
//!
//! The desired state records what the user asked for; the observed stage
//! records what the engine is actually doing (architecture brief section
//! 7.1). A pause or cancel request is accepted immediately, but an active
//! stage only leaves once the coordinator reports that work has stopped, so
//! the job shows as pausing or stopping in between.
//!
//! Two counters guard concurrent change:
//!
//! * the [`Revision`] advances on every accepted change, and user requests
//!   must present the revision they last saw;
//! * the [`Generation`] advances whenever in-flight work must be invalidated
//!   (cancel, retry, restart from scratch, and recovery after an engine
//!   restart). Stage reports must present the current generation, so a late
//!   result from old work is rejected (tests T17 and T27).

use std::fmt;
use std::str::FromStr;

use crate::version::{CounterExhausted, Generation, Revision};

/// What the user has asked the job to do.
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
pub enum DesiredState {
    /// Make progress whenever the job is eligible.
    Running,
    /// Stop at a safe checkpoint and stay stopped, keeping partial data.
    Paused,
    /// End the current operation and stop automatic retries, keeping data.
    Cancelled,
}

/// What the engine is observed to be doing with the job.
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
pub enum JobStage {
    /// Eligible and waiting for scheduler admission.
    Queued,
    /// A provider is resolving the resource into a transfer lease.
    Resolving,
    /// Bytes are being transferred into the owned partial.
    Transferring,
    /// Transferred bytes are being verified.
    Verifying,
    /// The verified artifact is being published under its root.
    Publishing,
    /// Reconciling a partial or publication after an engine restart or
    /// block, before any further transfer or publication.
    Recovering,
    /// Not eligible until a durable deadline or condition (cooldown, quota).
    Waiting,
    /// Waiting for a user answer, such as a challenge or password.
    NeedsInput,
    /// Stopped at a checkpoint by user request.
    Paused,
    /// Stopped by an environmental problem the user must fix, such as a full
    /// disk or missing mount.
    Blocked,
    /// Published and verified. Terminal.
    Complete,
    /// Failed without automatic retry; the user may retry.
    Failed,
    /// Cancelled by user request; the user may retry.
    Cancelled,
}

impl JobStage {
    /// Stages where work for the current generation may be running and must
    /// stop before the job can pause or cancel.
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Resolving
                | Self::Transferring
                | Self::Verifying
                | Self::Publishing
                | Self::Recovering
        )
    }

    /// Stages that perform provider or transfer work and therefore require
    /// [`DesiredState::Running`] to enter.
    const fn requires_running(self) -> bool {
        matches!(
            self,
            Self::Resolving | Self::Transferring | Self::Verifying | Self::Publishing
        )
    }

    /// Stable lowercase name, for storage and display.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Whether the coordinator may move a job from `self` to `to`.
    ///
    /// This is the complete transition table; anything not listed is
    /// rejected. Entering [`JobStage::Recovering`] from an active stage only
    /// happens through [`JobState::recover_after_restart`].
    pub const fn can_transition_to(self, to: Self) -> bool {
        use JobStage::*;
        matches!(
            (self, to),
            (Queued, Resolving | Waiting | Blocked | Paused | Cancelled)
                | (
                    Resolving,
                    Transferring | Queued | Waiting | NeedsInput | Blocked | Failed | Paused
                        | Cancelled
                )
                | (
                    Transferring,
                    Verifying | Resolving | Waiting | Blocked | Failed | Paused | Cancelled
                )
                | (Verifying, Publishing | Queued | Blocked | Failed | Paused | Cancelled)
                // Publication is not interruptible; pause and cancel wait for it.
                | (Publishing, Complete | Blocked | Failed)
                | (
                    Recovering,
                    Transferring | Verifying | Publishing | Complete | Queued | Blocked | Failed
                        | Paused | Cancelled
                )
                | (Waiting, Queued | Paused | Cancelled)
                | (NeedsInput, Queued | Failed | Paused | Cancelled)
                | (Blocked, Queued | Recovering | Failed | Paused | Cancelled)
                | (Paused, Queued | Cancelled)
                | (Failed, Queued | Cancelled)
                | (Cancelled, Queued)
        )
    }
}

/// What a client should show for a job. Its text form is `pausing`,
/// `stopping`, or the stage name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "String", into = "&'static str")
)]
pub enum DisplayState {
    /// The observed stage, with no pending stop request.
    Stage(JobStage),
    /// A pause was accepted; work is still stopping.
    Pausing,
    /// A cancel was accepted; work is still stopping.
    Stopping,
}

impl DisplayState {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stage(stage) => stage.as_str(),
            Self::Pausing => "pausing",
            Self::Stopping => "stopping",
        }
    }
}

impl fmt::Display for DisplayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<DisplayState> for &'static str {
    fn from(value: DisplayState) -> Self {
        value.as_str()
    }
}

impl FromStr for DisplayState {
    type Err = strum::ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pausing" => Ok(Self::Pausing),
            "stopping" => Ok(Self::Stopping),
            other => other.parse().map(Self::Stage),
        }
    }
}

impl TryFrom<String> for DisplayState {
    type Error = strum::ParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Whether an accepted request changed anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The state changed and the revision advanced.
    Changed,
    /// The job was already in the requested state; nothing changed.
    Unchanged,
}

/// A request or stage report was rejected. The state is left unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The request was based on an older revision.
    #[error("revision conflict: job is at {current}, request expected {expected}")]
    RevisionConflict {
        /// The job's current revision.
        current: Revision,
        /// The revision the caller presented.
        expected: Revision,
    },
    /// A stage report came from work of an older generation.
    #[error("stale generation {presented}; job is at generation {current}")]
    StaleGeneration {
        /// The job's current generation.
        current: Generation,
        /// The generation the caller presented.
        presented: Generation,
    },
    /// The transition is not in the stage table.
    #[error("invalid transition from {from} to {to}")]
    InvalidTransition {
        /// The current stage.
        from: JobStage,
        /// The requested stage.
        to: JobStage,
    },
    /// The stage requires a different desired state, such as entering
    /// `Transferring` while a pause is pending.
    #[error("cannot enter {to} while desired state is {desired}")]
    DesiredStateMismatch {
        /// The current desired state.
        desired: DesiredState,
        /// The requested stage.
        to: JobStage,
    },
    /// The request does not apply in the current state, such as pausing a
    /// completed job or retrying one that has not failed.
    #[error("request does not apply to a {stage} job with desired state {desired}")]
    NotApplicable {
        /// The current desired state.
        desired: DesiredState,
        /// The current stage.
        stage: JobStage,
    },
    /// The job is publishing; pause and cancel are accepted once
    /// publication settles.
    #[error("job is publishing; try again once it settles")]
    PublicationInProgress,
    /// A stored state violates the desired/stage invariants.
    #[error("inconsistent stored state: stage {stage} with desired state {desired}")]
    Inconsistent {
        /// The stored desired state.
        desired: DesiredState,
        /// The stored stage.
        stage: JobStage,
    },
    /// A revision or generation counter cannot advance.
    #[error(transparent)]
    CounterExhausted(#[from] CounterExhausted),
}

/// The mutable lifecycle state of one file job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobState {
    desired: DesiredState,
    stage: JobStage,
    generation: Generation,
    revision: Revision,
}

impl Default for JobState {
    fn default() -> Self {
        Self::new()
    }
}

impl JobState {
    /// A newly accepted job: running, queued, first generation and revision.
    pub const fn new() -> Self {
        Self {
            desired: DesiredState::Running,
            stage: JobStage::Queued,
            generation: Generation::INITIAL,
            revision: Revision::INITIAL,
        }
    }

    /// Restores a stored state, checking the desired/stage invariants:
    /// a paused stage needs a pause request, a cancelled stage needs a
    /// cancel request, and settled stages (complete, failed) carry no
    /// pending request.
    pub fn restore(
        desired: DesiredState,
        stage: JobStage,
        generation: Generation,
        revision: Revision,
    ) -> Result<Self, JobError> {
        let consistent = match stage {
            JobStage::Paused => desired == DesiredState::Paused,
            JobStage::Cancelled => desired == DesiredState::Cancelled,
            JobStage::Complete | JobStage::Failed => desired == DesiredState::Running,
            JobStage::Resolving
            | JobStage::Transferring
            | JobStage::Verifying
            | JobStage::Publishing => {
                // An active stage may have a pending stop request, except
                // publishing, which refuses them.
                stage != JobStage::Publishing || desired == DesiredState::Running
            }
            JobStage::Queued | JobStage::Waiting | JobStage::NeedsInput | JobStage::Blocked => {
                desired == DesiredState::Running
            }
            JobStage::Recovering => true,
        };
        if !consistent {
            return Err(JobError::Inconsistent { desired, stage });
        }
        Ok(Self {
            desired,
            stage,
            generation,
            revision,
        })
    }

    /// The user's requested state.
    pub const fn desired(&self) -> DesiredState {
        self.desired
    }

    /// The observed stage.
    pub const fn stage(&self) -> JobStage {
        self.stage
    }

    /// The current execution generation.
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    /// The current revision.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// What a client should show.
    pub const fn display(&self) -> DisplayState {
        if self.stage.is_active() {
            match self.desired {
                DesiredState::Paused => return DisplayState::Pausing,
                DesiredState::Cancelled => return DisplayState::Stopping,
                DesiredState::Running => {}
            }
        }
        DisplayState::Stage(self.stage)
    }

    fn check_revision(&self, expected: Revision) -> Result<(), JobError> {
        if expected != self.revision {
            return Err(JobError::RevisionConflict {
                current: self.revision,
                expected,
            });
        }
        Ok(())
    }

    fn not_applicable(&self) -> JobError {
        JobError::NotApplicable {
            desired: self.desired,
            stage: self.stage,
        }
    }

    /// Applies a change atomically: computes the new state, then commits it
    /// with an advanced revision, or leaves `self` untouched on error.
    fn commit(&mut self, mut next: Self) -> Result<Outcome, JobError> {
        next.revision = self.revision.next()?;
        *self = next;
        Ok(Outcome::Changed)
    }

    /// Requests a pause. Parked jobs pause immediately; active jobs keep
    /// their stage until the coordinator reports [`JobStage::Paused`].
    pub fn request_pause(&mut self, expected: Revision) -> Result<Outcome, JobError> {
        self.check_revision(expected)?;
        match (self.desired, self.stage) {
            (DesiredState::Paused, _) => Ok(Outcome::Unchanged),
            (DesiredState::Cancelled, _) | (_, JobStage::Complete | JobStage::Failed) => {
                Err(self.not_applicable())
            }
            (_, JobStage::Publishing) => Err(JobError::PublicationInProgress),
            (DesiredState::Running, stage) => {
                let mut next = *self;
                next.desired = DesiredState::Paused;
                if !stage.is_active() {
                    next.stage = JobStage::Paused;
                }
                self.commit(next)
            }
        }
    }

    /// Withdraws a pause. A paused job is queued again; a job still pausing
    /// simply continues.
    pub fn request_resume(&mut self, expected: Revision) -> Result<Outcome, JobError> {
        self.check_revision(expected)?;
        match self.desired {
            DesiredState::Running => Ok(Outcome::Unchanged),
            DesiredState::Cancelled => Err(self.not_applicable()),
            DesiredState::Paused => {
                let mut next = *self;
                next.desired = DesiredState::Running;
                if self.stage == JobStage::Paused {
                    next.stage = JobStage::Queued;
                }
                self.commit(next)
            }
        }
    }

    /// Requests cancellation. The generation advances immediately, so any
    /// result still in flight is rejected. Parked jobs are cancelled at
    /// once; active jobs stay in their stage until the coordinator reports
    /// [`JobStage::Cancelled`] under the new generation.
    pub fn request_cancel(&mut self, expected: Revision) -> Result<Outcome, JobError> {
        self.check_revision(expected)?;
        match (self.desired, self.stage) {
            (DesiredState::Cancelled, _) => Ok(Outcome::Unchanged),
            (_, JobStage::Complete) => Err(self.not_applicable()),
            (_, JobStage::Publishing) => Err(JobError::PublicationInProgress),
            (_, stage) => {
                let mut next = *self;
                next.desired = DesiredState::Cancelled;
                next.generation = self.generation.next()?;
                if !stage.is_active() {
                    next.stage = JobStage::Cancelled;
                }
                self.commit(next)
            }
        }
    }

    /// Retries a failed or cancelled job under a new generation.
    pub fn request_retry(&mut self, expected: Revision) -> Result<Outcome, JobError> {
        self.check_revision(expected)?;
        if !matches!(self.stage, JobStage::Failed | JobStage::Cancelled) {
            return Err(self.not_applicable());
        }
        let mut next = *self;
        next.desired = DesiredState::Running;
        next.stage = JobStage::Queued;
        next.generation = self.generation.next()?;
        self.commit(next)
    }

    /// Records a stage change reported by the coordinator for work of the
    /// `presented` generation.
    ///
    /// Restarting from scratch (verification or recovery back to
    /// [`JobStage::Queued`]) advances the generation so the old partial and
    /// any late worker lose their authority.
    pub fn advance(&mut self, to: JobStage, presented: Generation) -> Result<Outcome, JobError> {
        if presented != self.generation {
            return Err(JobError::StaleGeneration {
                current: self.generation,
                presented,
            });
        }
        if !self.stage.can_transition_to(to) {
            return Err(JobError::InvalidTransition {
                from: self.stage,
                to,
            });
        }
        let required = match to {
            JobStage::Paused => Some(DesiredState::Paused),
            JobStage::Cancelled => Some(DesiredState::Cancelled),
            stage if stage.requires_running() => Some(DesiredState::Running),
            // Settled or parked stages: the coordinator must finish stopping
            // a pending request instead of parking the job elsewhere.
            JobStage::Queued
            | JobStage::Waiting
            | JobStage::NeedsInput
            | JobStage::Blocked
            | JobStage::Complete
            | JobStage::Failed => Some(DesiredState::Running),
            _ => None,
        };
        if let Some(required) = required
            && self.desired != required
        {
            return Err(JobError::DesiredStateMismatch {
                desired: self.desired,
                to,
            });
        }
        let mut next = *self;
        next.stage = to;
        if to == JobStage::Queued
            && matches!(self.stage, JobStage::Verifying | JobStage::Recovering)
        {
            next.generation = self.generation.next()?;
        }
        self.commit(next)
    }

    /// Reconciles a stored state after the engine restarted. Work from the
    /// previous process is gone, so its generation is retired:
    ///
    /// * transferring, verifying, or publishing jobs move to
    ///   [`JobStage::Recovering`] to reconcile their partial or publication,
    ///   never straight back to the queue;
    /// * resolving jobs, which hold no durable bytes, settle according to
    ///   their desired state.
    ///
    /// Other stages are already durable and are left unchanged.
    pub fn recover_after_restart(&mut self) -> Result<Outcome, JobError> {
        let to = match self.stage {
            JobStage::Transferring | JobStage::Verifying | JobStage::Publishing => {
                JobStage::Recovering
            }
            JobStage::Resolving => match self.desired {
                DesiredState::Running => JobStage::Queued,
                DesiredState::Paused => JobStage::Paused,
                DesiredState::Cancelled => JobStage::Cancelled,
            },
            _ => return Ok(Outcome::Unchanged),
        };
        let mut next = *self;
        next.stage = to;
        next.generation = self.generation.next()?;
        self.commit(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use JobStage::*;
    use strum::VariantArray;

    const ALL: &[JobStage] = JobStage::VARIANTS;

    fn at(stage: JobStage, desired: DesiredState) -> JobState {
        JobState::restore(desired, stage, Generation::INITIAL, Revision::INITIAL).unwrap()
    }

    fn running(stage: JobStage) -> JobState {
        at(stage, DesiredState::Running)
    }

    /// A job at `stage` with the desired state that stage requires.
    fn settled(stage: JobStage) -> JobState {
        match stage {
            Paused => at(stage, DesiredState::Paused),
            Cancelled => at(stage, DesiredState::Cancelled),
            _ => running(stage),
        }
    }

    /// Drives a new job through the happy path.
    #[test]
    fn happy_path_completes() {
        let mut job = JobState::new();
        let g = job.generation();
        for stage in [Resolving, Transferring, Verifying, Publishing, Complete] {
            assert_eq!(job.advance(stage, g), Ok(Outcome::Changed));
        }
        assert_eq!(job.stage(), Complete);
        assert_eq!(job.generation(), g);
        assert_eq!(job.revision().get(), 6);
    }

    #[test]
    fn complete_is_terminal() {
        for &to in ALL {
            assert!(!Complete.can_transition_to(to), "complete -> {to}");
        }
        let mut job = running(Complete);
        let r = job.revision();
        assert!(job.request_pause(r).is_err());
        assert!(job.request_cancel(r).is_err());
        assert!(job.request_retry(r).is_err());
        assert_eq!(job, running(Complete));
    }

    #[test]
    fn rejects_transitions_outside_the_table() {
        let cases = [
            (Queued, Transferring),
            (Queued, Complete),
            (Resolving, Publishing),
            (Transferring, Complete),
            (Transferring, Queued),
            (Verifying, Complete),
            (Waiting, Resolving),
            (Paused, Resolving),
            (Failed, Resolving),
            (Queued, Recovering),
            (Transferring, Recovering),
        ];
        for (from, to) in cases {
            let mut job = settled(from);
            let before = job;
            assert_eq!(
                job.advance(to, job.generation()),
                Err(JobError::InvalidTransition { from, to }),
            );
            assert_eq!(job, before, "rejected transition changed state");
        }
    }

    #[test]
    fn no_stage_transitions_to_itself() {
        for &stage in ALL {
            assert!(!stage.can_transition_to(stage), "{stage} -> itself");
        }
    }

    #[test]
    fn stale_generation_is_rejected() {
        let mut job = running(Transferring);
        let old = job.generation();
        job.request_cancel(job.revision()).unwrap();
        let before = job;
        assert_eq!(
            job.advance(Verifying, old),
            Err(JobError::StaleGeneration {
                current: job.generation(),
                presented: old
            }),
        );
        assert_eq!(job, before);
    }

    #[test]
    fn stale_revision_is_rejected() {
        let mut job = running(Queued);
        let stale = job.revision();
        job.advance(Resolving, job.generation()).unwrap();
        let before = job;
        assert!(matches!(
            job.request_pause(stale),
            Err(JobError::RevisionConflict { .. })
        ));
        assert_eq!(job, before);
    }

    #[test]
    fn pause_of_active_job_waits_for_checkpoint() {
        let mut job = running(Transferring);
        let g = job.generation();
        job.request_pause(job.revision()).unwrap();
        assert_eq!(job.stage(), Transferring);
        assert_eq!(job.display(), DisplayState::Pausing);
        // The draining worker may not start new work while pausing.
        assert!(matches!(
            job.advance(Verifying, g),
            Err(JobError::DesiredStateMismatch { .. })
        ));
        // Pausing keeps the generation, so the worker can checkpoint and stop.
        job.advance(Paused, g).unwrap();
        assert_eq!(job.display(), DisplayState::Stage(Paused));
        job.request_resume(job.revision()).unwrap();
        assert_eq!(job.stage(), Queued);
        assert_eq!(job.generation(), g);
    }

    #[test]
    fn pause_of_parked_job_is_immediate() {
        for stage in [Queued, Waiting, NeedsInput, Blocked] {
            let mut job = running(stage);
            job.request_pause(job.revision()).unwrap();
            assert_eq!(job.stage(), Paused, "from {stage}");
        }
    }

    #[test]
    fn resume_while_pausing_continues_work() {
        let mut job = running(Transferring);
        job.request_pause(job.revision()).unwrap();
        job.request_resume(job.revision()).unwrap();
        assert_eq!(job.display(), DisplayState::Stage(Transferring));
        assert_eq!(
            job.advance(Verifying, job.generation()),
            Ok(Outcome::Changed)
        );
    }

    #[test]
    fn cancel_invalidates_in_flight_work() {
        let mut job = running(Resolving);
        let old = job.generation();
        job.request_cancel(job.revision()).unwrap();
        assert_eq!(job.display(), DisplayState::Stopping);
        assert!(job.generation() > old);
        // The late resolve result is rejected.
        assert!(matches!(
            job.advance(Transferring, old),
            Err(JobError::StaleGeneration { .. })
        ));
        // The coordinator confirms the stop under the new generation.
        job.advance(Cancelled, job.generation()).unwrap();
        assert_eq!(job.display(), DisplayState::Stage(Cancelled));
    }

    #[test]
    fn cancel_of_parked_job_is_immediate_and_retires_generation() {
        let mut job = running(Waiting);
        let old = job.generation();
        job.request_cancel(job.revision()).unwrap();
        assert_eq!(job.stage(), Cancelled);
        assert!(job.generation() > old);
    }

    #[test]
    fn publication_refuses_pause_and_cancel() {
        let mut job = running(Publishing);
        let r = job.revision();
        assert_eq!(job.request_pause(r), Err(JobError::PublicationInProgress));
        assert_eq!(job.request_cancel(r), Err(JobError::PublicationInProgress));
        assert_eq!(job, running(Publishing));
    }

    #[test]
    fn repeated_requests_are_unchanged() {
        let mut job = running(Queued);
        job.request_pause(job.revision()).unwrap();
        let r = job.revision();
        assert_eq!(job.request_pause(r), Ok(Outcome::Unchanged));
        assert_eq!(job.revision(), r);
        job.request_cancel(r).unwrap();
        let r = job.revision();
        assert_eq!(job.request_cancel(r), Ok(Outcome::Unchanged));
        assert_eq!(job.request_resume(r), Err(job.not_applicable()));
    }

    #[test]
    fn retry_requires_failed_or_cancelled_and_advances_generation() {
        for stage in [Failed, Cancelled] {
            let desired = if stage == Cancelled {
                DesiredState::Cancelled
            } else {
                DesiredState::Running
            };
            let mut job = at(stage, desired);
            let old = job.generation();
            job.request_retry(job.revision()).unwrap();
            assert_eq!(job.stage(), Queued);
            assert_eq!(job.desired(), DesiredState::Running);
            assert!(job.generation() > old);
        }
        for stage in [Queued, Transferring, Waiting, Complete] {
            let mut job = running(stage);
            assert!(
                job.request_retry(job.revision()).is_err(),
                "retry from {stage}"
            );
        }
    }

    #[test]
    fn coordinator_cannot_park_a_job_with_a_pending_stop() {
        let mut job = running(Transferring);
        job.request_cancel(job.revision()).unwrap();
        let g = job.generation();
        for to in [Waiting, Blocked, Failed, Paused] {
            let mut j = job;
            assert!(j.advance(to, g).is_err(), "cancelling job moved to {to}");
        }
    }

    #[test]
    fn coordinator_cannot_cancel_or_pause_on_its_own() {
        let mut job = running(Transferring);
        let g = job.generation();
        assert!(matches!(
            job.advance(Cancelled, g),
            Err(JobError::DesiredStateMismatch { .. })
        ));
        assert!(matches!(
            job.advance(Paused, g),
            Err(JobError::DesiredStateMismatch { .. })
        ));
    }

    #[test]
    fn restart_from_scratch_retires_the_generation() {
        let mut job = running(Verifying);
        let old = job.generation();
        job.advance(Queued, old).unwrap();
        assert!(job.generation() > old);

        let mut job = running(Transferring);
        let old = job.generation();
        // Re-resolving an expired lease keeps the partial and generation.
        job.advance(Resolving, old).unwrap();
        assert_eq!(job.generation(), old);
    }

    #[test]
    fn recovery_after_restart_never_requeues_transfers() {
        for stage in [Transferring, Verifying, Publishing] {
            let mut job = running(stage);
            let old = job.generation();
            job.recover_after_restart().unwrap();
            assert_eq!(job.stage(), Recovering, "from {stage}");
            assert!(job.generation() > old);
            // An old worker that survived the restart cannot report.
            assert!(job.advance(Complete, old).is_err());
        }
        let mut job = at(Resolving, DesiredState::Paused);
        job.recover_after_restart().unwrap();
        assert_eq!(job.stage(), Paused);

        let mut job = running(Waiting);
        assert_eq!(job.recover_after_restart(), Ok(Outcome::Unchanged));
    }

    #[test]
    fn recovering_job_honours_pending_cancel() {
        let mut job = at(Transferring, DesiredState::Cancelled);
        job.recover_after_restart().unwrap();
        assert_eq!(job.display(), DisplayState::Stopping);
        let g = job.generation();
        assert!(job.clone().advance(Transferring, g).is_err());
        job.advance(Cancelled, g).unwrap();
    }

    #[test]
    fn restore_rejects_inconsistent_state() {
        let g = Generation::INITIAL;
        let r = Revision::INITIAL;
        for (desired, stage) in [
            (DesiredState::Running, Paused),
            (DesiredState::Running, Cancelled),
            (DesiredState::Paused, Complete),
            (DesiredState::Cancelled, Queued),
            (DesiredState::Paused, Publishing),
        ] {
            assert!(
                JobState::restore(desired, stage, g, r).is_err(),
                "{desired:?}/{stage}"
            );
        }
    }

    /// Exhaustively checks that every stage reachable by `advance` from a
    /// consistent state yields a state `restore` accepts.
    #[test]
    fn transitions_preserve_invariants() {
        let desires = [
            DesiredState::Running,
            DesiredState::Paused,
            DesiredState::Cancelled,
        ];
        for &from in ALL {
            for desired in desires {
                let Ok(job) =
                    JobState::restore(desired, from, Generation::INITIAL, Revision::INITIAL)
                else {
                    continue;
                };
                for &to in ALL {
                    let mut next = job;
                    if next.advance(to, next.generation()).is_ok() {
                        JobState::restore(next.desired, next.stage, next.generation, next.revision)
                            .unwrap_or_else(|e| panic!("{from}/{desired:?} -> {to}: {e}"));
                    }
                }
                let mut recovered = job;
                if recovered.recover_after_restart().is_ok() {
                    JobState::restore(
                        recovered.desired,
                        recovered.stage,
                        recovered.generation,
                        recovered.revision,
                    )
                    .unwrap();
                }
            }
        }
    }
}
