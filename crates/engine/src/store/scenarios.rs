//! Store behavior across commands, staging, transitions, recovery, and
//! checkpoints.

use std::fs::File;

use tempfile::TempDir;
use wayfetch_domain::failure::{Failure, FailureCode, FailureScope};
use wayfetch_domain::id::{AttemptId, JobId, PackageId};
use wayfetch_domain::job::{DesiredState, DisplayState, JobStage, Outcome};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{Generation, Revision};
use wayfetch_protocol::command::{InputUrl, SubmitItems};
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::query::{JobFilter, JobSummary, PageSize};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, IdempotencyKey, InstanceId, Query,
    QueryResult,
};

use super::progress::SyncedRanges;
use super::{Store, StoreError, outbox};

const NOW: UtcTimestamp = UtcTimestamp::from_unix_millis(1_700_000_000_000);

struct Fixture {
    dir: TempDir,
    store: Store,
    instance: InstanceId,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        Self {
            dir,
            store,
            instance: InstanceId::generate(),
        }
    }

    fn reopen(self) -> Self {
        let Self {
            dir,
            store,
            instance,
        } = self;
        drop(store);
        let store = Store::open(dir.path()).unwrap();
        Self {
            dir,
            store,
            instance,
        }
    }

    fn run(&mut self, key: IdempotencyKey, command: Command) -> Result<CommandResult, ErrorCode> {
        let envelope = CommandEnvelope {
            idempotency_key: key,
            target: self.instance,
            command,
        };
        self.store
            .execute(&envelope, self.instance, NOW)
            .map_err(|e| e.code)
    }

    fn submit(&mut self, urls: &[&str]) -> CommandResult {
        self.run(IdempotencyKey::generate(), submit_command(urls, None))
            .unwrap()
    }

    /// Submits one URL and stages it into a job.
    fn job(&mut self) -> JobId {
        self.submit(&["https://files.example/a.bin"]);
        let staged = self.store.stage_pending(10, NOW).unwrap();
        assert_eq!(staged.len(), 1);
        staged[0]
    }

    fn summary(&mut self, job: JobId) -> JobSummary {
        match self
            .store
            .query(&Query::InspectJob { job }, self.instance)
            .unwrap()
        {
            QueryResult::Job { job, .. } => job,
            other => panic!("{other:?}"),
        }
    }

    fn events(&mut self) -> Vec<EventKind> {
        let tx = self.store.read().unwrap();
        outbox::after(&tx, 0).into_iter().map(|(_, k)| k).collect()
    }

    fn advance(&mut self, job: JobId, to: JobStage) -> Result<Outcome, StoreError> {
        let generation = self.summary(job).generation;
        self.store.advance_job(job, to, generation, None, NOW)
    }

    fn checkpoint(
        &mut self,
        job: JobId,
        generation: Generation,
        ranges: &[(u64, u64)],
    ) -> Result<u64, StoreError> {
        let file = File::create(self.dir.path().join("partial")).unwrap();
        let ranges = ranges.iter().map(|&(start, end)| start..end).collect();
        let synced = SyncedRanges::after_sync(&file, job, generation, ranges).unwrap();
        self.store.commit_checkpoint(synced)
    }
}

fn submit_command(urls: &[&str], package: Option<PackageId>) -> Command {
    let items: Vec<_> = urls.iter().map(|u| InputUrl::parse(u).unwrap()).collect();
    Command::Submit {
        items: SubmitItems::try_from(items).unwrap(),
        package,
    }
}

fn rev(n: u64) -> Revision {
    Revision::from_u64(n).unwrap()
}

#[test]
fn submit_is_durable_and_replays_after_reopening() {
    let mut f = Fixture::new();
    let key = IdempotencyKey::generate();
    let command = submit_command(&["https://a.example/1", "https://a.example/2"], None);
    let first = f.run(key, command.clone()).unwrap();
    let CommandResult::Submitted { batch, accepted } = first else {
        panic!("{first:?}")
    };
    assert_eq!(accepted, 2);
    assert_eq!(
        f.events(),
        vec![EventKind::BatchAccepted { batch, accepted }]
    );

    // A retry after a restart returns the stored result and changes nothing.
    let mut f = f.reopen();
    assert_eq!(f.run(key, command).unwrap(), first);
    assert_eq!(f.events().len(), 1);

    // The same key with another command is a conflict.
    let other = submit_command(&["https://a.example/3"], None);
    assert_eq!(f.run(key, other), Err(ErrorCode::IdempotencyConflict));
}

#[test]
fn rejected_commands_leave_no_record() {
    let mut f = Fixture::new();
    let key = IdempotencyKey::generate();
    let missing = PackageId::from_u64(99).unwrap();
    let command = submit_command(&["https://a.example/1"], Some(missing));
    assert_eq!(f.run(key, command), Err(ErrorCode::NotFound));
    assert!(f.events().is_empty());
    // Nothing was recorded under the key, so it can carry a valid command.
    assert!(
        f.run(key, submit_command(&["https://a.example/1"], None))
            .is_ok()
    );

    let envelope = CommandEnvelope {
        idempotency_key: IdempotencyKey::generate(),
        target: InstanceId::generate(),
        command: submit_command(&["https://a.example/1"], None),
    };
    let err = f.store.execute(&envelope, f.instance, NOW).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongInstance);
}

#[test]
fn staging_creates_one_job_per_link_and_can_repeat() {
    let mut f = Fixture::new();
    let CommandResult::Submitted { .. } = f.submit(&["https://a.example/1", "https://b.example/2"])
    else {
        unreachable!()
    };
    let first = f.store.stage_pending(1, NOW).unwrap();
    let second = f.store.stage_pending(10, NOW).unwrap();
    assert_eq!((first.len(), second.len()), (1, 1));
    assert!(f.store.stage_pending(10, NOW).unwrap().is_empty());

    let summary = f.summary(first[0]);
    assert_eq!(summary.stage, JobStage::Queued);
    assert_eq!(summary.desired, DesiredState::Running);
    assert_eq!(summary.revision, Revision::INITIAL);
    let added = f
        .events()
        .into_iter()
        .filter(|e| matches!(e, EventKind::JobAdded { .. }))
        .count();
    assert_eq!(added, 2);
}

#[test]
fn job_requests_are_revision_checked() {
    let mut f = Fixture::new();
    let job = f.job();
    f.advance(job, JobStage::Resolving).unwrap();
    let current = f.summary(job).revision;

    let stale = Command::Pause {
        job,
        expected_revision: rev(1),
    };
    assert_eq!(
        f.run(IdempotencyKey::generate(), stale),
        Err(ErrorCode::RevisionConflict)
    );

    let pause = Command::Pause {
        job,
        expected_revision: current,
    };
    let CommandResult::JobUpdated {
        revision, changed, ..
    } = f.run(IdempotencyKey::generate(), pause).unwrap()
    else {
        unreachable!()
    };
    assert!(changed);
    // An active job keeps working until the worker stops.
    let summary = f.summary(job);
    assert_eq!(summary.revision, revision);
    assert_eq!(summary.display, DisplayState::Pausing);
    assert!(
        matches!(f.events().last(), Some(EventKind::JobChanged { job }) if job.revision == revision)
    );

    let missing = Command::Cancel {
        job: JobId::from_u64(u64::MAX).unwrap(),
        expected_revision: rev(1),
    };
    assert_eq!(
        f.run(IdempotencyKey::generate(), missing),
        Err(ErrorCode::NotFound)
    );
}

#[test]
fn stale_generations_cannot_advance_a_job() {
    let mut f = Fixture::new();
    let job = f.job();
    let old = f.summary(job).generation;
    f.advance(job, JobStage::Resolving).unwrap();
    let cancel = Command::Cancel {
        job,
        expected_revision: f.summary(job).revision,
    };
    f.run(IdempotencyKey::generate(), cancel).unwrap();
    let current = f.summary(job).generation;
    assert_ne!(current, old);

    // A late result from the cancelled run is rejected and changes nothing.
    let before = f.summary(job);
    let err = f
        .store
        .advance_job(job, JobStage::Transferring, old, None, NOW)
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Job(wayfetch_domain::job::JobError::StaleGeneration { .. })
    ));
    assert_eq!(f.summary(job), before);
}

#[test]
fn parked_stages_carry_their_failure() {
    let mut f = Fixture::new();
    let job = f.job();
    f.advance(job, JobStage::Resolving).unwrap();
    let generation = f.summary(job).generation;
    assert!(matches!(
        f.store
            .advance_job(job, JobStage::Waiting, generation, None, NOW),
        Err(StoreError::MissingFailure(JobStage::Waiting))
    ));

    let retry_at = UtcTimestamp::from_unix_millis(1_700_000_060_000);
    let failure = Failure::new(
        FailureCode::RequestTimeout,
        FailureScope::Attempt(AttemptId::from_u64(1).unwrap()),
    )
    .unwrap()
    .with_earliest_retry(retry_at)
    .with_context(DiagnosticText::new("no response"));
    f.store
        .advance_job(job, JobStage::Waiting, generation, Some(failure), NOW)
        .unwrap();
    let parked = f.summary(job).failure.unwrap();
    assert_eq!(parked.code, FailureCode::RequestTimeout);
    assert_eq!(parked.retry_at, Some(retry_at));
    assert_eq!(parked.message.unwrap().as_str(), "no response");

    // Leaving the parked stage clears it.
    f.advance(job, JobStage::Queued).unwrap();
    assert_eq!(f.summary(job).failure, None);
}

#[test]
fn restart_moves_active_jobs_to_recovery_under_a_new_generation() {
    let mut f = Fixture::new();
    let active = f.job();
    let queued = f.job();
    f.advance(active, JobStage::Resolving).unwrap();
    f.advance(active, JobStage::Transferring).unwrap();
    let before = f.summary(active);

    let mut f = f.reopen();
    assert_eq!(f.store.recover_after_restart(NOW).unwrap(), 1);
    let after = f.summary(active);
    assert_eq!(after.stage, JobStage::Recovering);
    assert_ne!(after.generation, before.generation);
    assert_eq!(f.summary(queued).stage, JobStage::Queued);

    // Work authorized before the restart is retired (T27).
    assert!(
        f.store
            .advance_job(active, JobStage::Verifying, before.generation, None, NOW)
            .is_err()
    );
    assert!(f.checkpoint(active, before.generation, &[(0, 10)]).is_err());
    // Restarting again during recovery retires that generation too.
    assert_eq!(f.store.recover_after_restart(NOW).unwrap(), 1);
    assert_eq!(
        f.summary(active).generation,
        after.generation.next().unwrap()
    );
}

#[test]
fn checkpoints_track_synchronized_coverage_not_file_length() {
    let mut f = Fixture::new();
    let job = f.job();
    f.advance(job, JobStage::Resolving).unwrap();
    // Only a transferring job takes checkpoints.
    let generation = f.summary(job).generation;
    assert!(matches!(
        f.checkpoint(job, generation, &[(0, 10)]),
        Err(StoreError::Checkpoint(_))
    ));
    f.advance(job, JobStage::Transferring).unwrap();

    // A hole between ranges is not coverage (T25).
    assert_eq!(
        f.checkpoint(job, generation, &[(0, 10), (20, 30)]).unwrap(),
        20
    );
    assert_eq!(f.summary(job).bytes_done, 20);
    // Adjacent ranges merge; overlapping ones are refused and change nothing.
    assert_eq!(f.checkpoint(job, generation, &[(10, 20)]).unwrap(), 30);
    for bad in [&[(25, 40)][..], &[(30, 40), (35, 45)], &[(40, 40)], &[]] {
        assert!(matches!(
            f.checkpoint(job, generation, bad),
            Err(StoreError::Checkpoint(_))
        ));
    }
    let beyond = u64::try_from(i64::MAX).unwrap() + 1;
    assert!(f.checkpoint(job, generation, &[(40, beyond)]).is_err());
    assert_eq!(f.summary(job).bytes_done, 30);

    let tx = f.store.read().unwrap();
    let (seq, ranges): (i64, i64) = tx
        .query_row(
            "SELECT checkpoint_seq, (SELECT count(*) FROM durable_ranges r \
             WHERE r.partial_id = p.id) FROM partials p",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((seq, ranges), (2, 1));
}

#[test]
fn job_pages_are_keyset_ordered_and_filtered() {
    let mut f = Fixture::new();
    let jobs: Vec<JobId> = (0..5).map(|_| f.job()).collect();
    f.advance(jobs[1], JobStage::Resolving).unwrap();
    f.advance(jobs[3], JobStage::Resolving).unwrap();

    let list = |f: &mut Fixture, after, stages: Vec<JobStage>| {
        let query = Query::ListJobs {
            filter: JobFilter {
                stages,
                package: None,
            },
            after,
            page_size: PageSize::try_from(2).unwrap(),
        };
        match f.store.query(&query, f.instance).unwrap() {
            QueryResult::Jobs(page) => page,
            other => panic!("{other:?}"),
        }
    };
    let first = list(&mut f, None, vec![]);
    assert_eq!(
        first.items.iter().map(|j| j.id).collect::<Vec<_>>(),
        jobs[..2]
    );
    assert_eq!(first.next_after, Some(jobs[1]));
    let tx = f.store.read().unwrap();
    assert_eq!(first.as_of.position, outbox::head(&tx).unwrap());
    drop(tx);
    let last = list(&mut f, Some(jobs[3]), vec![]);
    assert_eq!(last.items.len(), 1);
    assert_eq!(last.next_after, None);

    let resolving = list(&mut f, None, vec![JobStage::Resolving]);
    assert_eq!(
        resolving.items.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![jobs[1], jobs[3]]
    );
    let beyond = list(&mut f, Some(JobId::from_u64(u64::MAX).unwrap()), vec![]);
    assert!(beyond.items.is_empty());
}
