//! Store behavior across commands, staging, transitions, recovery,
//! checkpoints, and publication.

use std::collections::HashMap;

use tempfile::TempDir;
use wayfetch_domain::failure::{Failure, FailureCode, FailureScope};
use wayfetch_domain::id::{AttemptId, JobId, PackageId};
use wayfetch_domain::job::{DesiredState, DisplayState, JobStage, Outcome};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{BootEpoch, Generation, Revision};
use wayfetch_protocol::command::{InputUrl, SubmitItems};
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::query::{ArtifactSummary, JobFilter, JobSummary, PageSize};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, IdempotencyKey, InstanceId, Query,
    QueryResult,
};

use super::{Store, StoreError, outbox};
use crate::fs::{PublicationCheck, RelativePath, Root, StagingFile};
use crate::transfer::lease::Representation;

const NOW: UtcTimestamp = UtcTimestamp::from_unix_millis(1_700_000_000_000);

struct Fixture {
    dir: TempDir,
    store: Store,
    instance: InstanceId,
    /// A destination root and the staging files opened in it.
    root_dir: TempDir,
    root: Root,
    files: HashMap<(JobId, Generation), StagingFile>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (root_dir, root) = crate::fs::test_root();
        Self {
            dir,
            store,
            instance: InstanceId::generate(),
            root_dir,
            root,
            files: HashMap::new(),
        }
    }

    /// Simulates a process restart: the database is reopened and every
    /// open file handle is gone.
    fn reopen(mut self) -> Self {
        let store = std::mem::replace(&mut self.store, Store::open(self.dir.path()).unwrap());
        drop(store);
        self.files.clear();
        self
    }

    /// The staging file of `job` at `generation`, created if needed but not
    /// registered.
    fn file(&mut self, job: JobId, generation: Generation) -> &StagingFile {
        self.files
            .entry((job, generation))
            .or_insert_with(|| self.root.create_staging(job, generation).unwrap())
    }

    /// Creates and registers the staging file of a transferring job.
    fn register(&mut self, job: JobId, epoch: BootEpoch) -> Generation {
        let generation = self.summary(job).generation;
        let record = self.file(job, generation).record().clone();
        self.store.register_staging(&record, epoch, NOW).unwrap();
        generation
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
        outbox::all(&tx).into_iter().map(|(_, k)| k).collect()
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
        let ranges = ranges.iter().map(|&(start, end)| start..end).collect();
        let synced = self.file(job, generation).sync_ranges(ranges).unwrap();
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
    f.register(job, BootEpoch::INITIAL);

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

/// Moves a fresh job to transferring with a registered staging file.
fn transferring(f: &mut Fixture, epoch: BootEpoch) -> (JobId, Generation) {
    let job = f.job();
    f.advance(job, JobStage::Resolving).unwrap();
    f.advance(job, JobStage::Transferring).unwrap();
    let generation = f.register(job, epoch);
    (job, generation)
}

/// Writes and checkpoints `bytes` from offset 0, then moves the job to
/// publishing.
fn finished(f: &mut Fixture, job: JobId, generation: Generation, bytes: &[u8]) {
    f.file(job, generation).write_all_at(bytes, 0).unwrap();
    if !bytes.is_empty() {
        f.checkpoint(job, generation, &[(0, bytes.len() as u64)])
            .unwrap();
    }
    f.advance(job, JobStage::Verifying).unwrap();
    f.advance(job, JobStage::Publishing).unwrap();
}

fn path(p: &str) -> RelativePath {
    RelativePath::new(p).unwrap()
}

#[test]
fn checkpoints_need_the_generations_registered_file() {
    let mut f = Fixture::new();
    let job = f.job();
    f.advance(job, JobStage::Resolving).unwrap();
    f.advance(job, JobStage::Transferring).unwrap();
    let generation = f.summary(job).generation;
    // Bytes in a file nobody registered certify nothing.
    assert!(matches!(
        f.checkpoint(job, generation, &[(0, 10)]),
        Err(StoreError::Checkpoint(_))
    ));
    // Registration needs a transferring job at that generation.
    let stale = f.file(job, generation.next().unwrap()).record().clone();
    assert!(
        f.store
            .register_staging(&stale, BootEpoch::INITIAL, NOW)
            .is_err()
    );

    f.store
        .register_staging(
            f.files[&(job, generation)].record(),
            BootEpoch::INITIAL,
            NOW,
        )
        .unwrap();
    assert_eq!(f.checkpoint(job, generation, &[(0, 10)]).unwrap(), 10);
    // A second file for the same generation cannot be registered.
    let sibling = f.job();
    let other = f.root.create_staging(sibling, generation).unwrap();
    let mut forged = other.record().clone();
    forged.job = job;
    assert!(
        f.store
            .register_staging(&forged, BootEpoch::INITIAL, NOW)
            .is_err()
    );
}

#[test]
fn publication_reserves_its_target_and_completes_the_job() {
    let mut f = Fixture::new();
    let (job, generation) = transferring(&mut f, BootEpoch::INITIAL);
    finished(&mut f, job, generation, b"hello");

    // File length is not coverage: the artifact must be exactly 0..5.
    for size in [4, 6] {
        assert!(matches!(
            f.store
                .begin_publication(job, generation, &path("out/a.bin"), size, NOW),
            Err(StoreError::Staging(_))
        ));
    }
    f.store
        .begin_publication(job, generation, &path("out/a.bin"), 5, NOW)
        .unwrap();
    let intent = f.store.pending_publication(job).unwrap().unwrap();
    assert_eq!(intent.target, path("out/a.bin"));
    assert_eq!(intent.size, 5);
    assert_eq!(&intent.staging, f.files[&(job, generation)].record());

    // A sibling job cannot reserve the same name, even in another case (T31).
    let (sibling, sibling_gen) = transferring(&mut f, BootEpoch::INITIAL);
    finished(&mut f, sibling, sibling_gen, b"other");
    assert!(matches!(
        f.store
            .begin_publication(sibling, sibling_gen, &path("OUT/A.bin"), 5, NOW),
        Err(StoreError::TargetReserved(_))
    ));
    f.store
        .begin_publication(sibling, sibling_gen, &path("OUT/A (1).bin"), 5, NOW)
        .unwrap();

    let file = f.files.remove(&(job, generation)).unwrap();
    f.root.publish(file, &intent.target, intent.size).unwrap();
    f.store.complete_publication(job, generation, NOW).unwrap();
    let done = f.summary(job);
    assert_eq!(done.stage, JobStage::Complete);
    assert_eq!(
        done.artifact,
        Some(ArtifactSummary {
            root: f.root.id(),
            path: "out/a.bin".into(),
            size: 5,
        })
    );
    assert_eq!(f.summary(sibling).artifact, None);
    assert_eq!(f.store.pending_publication(job).unwrap(), None);
    assert_eq!(f.store.latest_staging(job).unwrap(), None);
    assert_eq!(
        std::fs::read(f.root_dir.path().join("out/a.bin")).unwrap(),
        b"hello"
    );
    // Completing twice is refused; the reservation outlives the intent.
    assert!(f.store.complete_publication(job, generation, NOW).is_err());
    assert!(matches!(
        f.store
            .begin_publication(sibling, sibling_gen, &path("out/a.bin"), 5, NOW),
        Err(StoreError::TargetReserved(_))
    ));
}

#[test]
fn a_valid_empty_file_publishes_without_coverage() {
    let mut f = Fixture::new();
    let (job, generation) = transferring(&mut f, BootEpoch::INITIAL);
    finished(&mut f, job, generation, b"");
    f.store
        .begin_publication(job, generation, &path("empty.txt"), 0, NOW)
        .unwrap();
    let file = f.files.remove(&(job, generation)).unwrap();
    f.root.publish(file, &path("empty.txt"), 0).unwrap();
    f.store.complete_publication(job, generation, NOW).unwrap();
    assert_eq!(
        std::fs::metadata(f.root_dir.path().join("empty.txt"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn restart_during_publication_reconciles_from_the_intent() {
    let mut f = Fixture::new();
    let (job, generation) = transferring(&mut f, BootEpoch::INITIAL);
    finished(&mut f, job, generation, b"data");
    f.store
        .begin_publication(job, generation, &path("a.bin"), 4, NOW)
        .unwrap();

    // Crash after the intent committed, before the link (T24).
    let mut f = f.reopen();
    f.store.recover_after_restart(NOW).unwrap();
    let current = f.summary(job).generation;
    assert_eq!(f.summary(job).stage, JobStage::Recovering);
    let intent = f.store.pending_publication(job).unwrap().unwrap();
    assert_eq!(
        f.root
            .reconcile_publication(&intent.staging, &intent.target, intent.size)
            .unwrap(),
        PublicationCheck::NotPublished
    );
    // The old generation can no longer complete it.
    assert!(f.store.complete_publication(job, generation, NOW).is_err());
    let file = f.root.reopen_staging(&intent.staging, intent.size).unwrap();
    f.root.publish(file, &intent.target, intent.size).unwrap();
    assert_eq!(
        f.root
            .reconcile_publication(&intent.staging, &intent.target, intent.size)
            .unwrap(),
        PublicationCheck::Published
    );
    f.store.complete_publication(job, current, NOW).unwrap();
    assert_eq!(f.summary(job).stage, JobStage::Complete);
}

#[test]
fn a_conflicting_target_releases_the_reservation() {
    let mut f = Fixture::new();
    let (job, generation) = transferring(&mut f, BootEpoch::INITIAL);
    finished(&mut f, job, generation, b"data");
    std::fs::write(f.root_dir.path().join("a.bin"), b"user file").unwrap();
    f.store
        .begin_publication(job, generation, &path("a.bin"), 4, NOW)
        .unwrap();
    let file = f.files.remove(&(job, generation)).unwrap();
    assert!(f.root.publish(file, &path("a.bin"), 4).is_err());
    let intent = f.store.pending_publication(job).unwrap().unwrap();
    assert_eq!(
        f.root
            .reconcile_publication(&intent.staging, &intent.target, intent.size)
            .unwrap(),
        PublicationCheck::Conflict
    );
    f.store.abandon_publication(job, generation).unwrap();
    f.store
        .begin_publication(job, generation, &path("a (1).bin"), 4, NOW)
        .unwrap();
    let file = f.root.reopen_staging(&intent.staging, 4).unwrap();
    f.root.publish(file, &path("a (1).bin"), 4).unwrap();
    f.store.complete_publication(job, generation, NOW).unwrap();
    assert_eq!(
        std::fs::read(f.root_dir.path().join("a.bin")).unwrap(),
        b"user file"
    );
}

#[test]
#[expect(clippy::single_range_in_vec_init, reason = "lists of one byte range")]
fn only_a_previous_starts_file_is_adopted() {
    let mut f = Fixture::new();
    let first_boot = BootEpoch::INITIAL;
    let (job, generation) = transferring(&mut f, first_boot);
    f.file(job, generation)
        .write_all_at(b"0123456789", 0)
        .unwrap();
    f.checkpoint(job, generation, &[(0, 6)]).unwrap();

    let mut f = f.reopen();
    f.store.recover_after_restart(NOW).unwrap();
    let current = f.summary(job).generation;
    let partial = f.store.latest_staging(job).unwrap().unwrap();
    let (record, durable_end) = (partial.record, partial.durable_end);
    assert_eq!((record.generation, durable_end), (generation, 6));
    assert!(partial.contiguous);
    assert_eq!(partial.representation, None);
    // Within the boot that created it, a live writer could still hold it.
    assert!(matches!(
        f.store.adopt_staging(job, current, first_boot),
        Err(StoreError::Staging(_))
    ));
    let second_boot = first_boot.next().unwrap();
    f.root.reopen_staging(&record, durable_end).unwrap();
    let adopted = f.store.adopt_staging(job, current, second_boot).unwrap();
    assert_eq!(adopted.generation, current);
    assert_eq!(adopted.identity, record.identity);
    let partial = f.store.latest_staging(job).unwrap().unwrap();
    assert_eq!((partial.record, partial.durable_end), (adopted.clone(), 6));
    assert_eq!(f.summary(job).bytes_done, 6);

    // The new generation continues the coverage; the old one cannot (T27).
    f.advance(job, JobStage::Transferring).unwrap();
    let file = f.root.reopen_staging(&adopted, 6).unwrap();
    let synced = file.sync_ranges(vec![6..10]).unwrap();
    assert_eq!(f.store.commit_checkpoint(synced).unwrap(), 10);
    let stale = f.root.reopen_staging(&record, 0).unwrap();
    assert!(
        f.store
            .commit_checkpoint(stale.sync_ranges(vec![10..12]).unwrap())
            .is_err()
    );
    // Adopting again finds nothing older to take.
    assert!(f.store.adopt_staging(job, current, second_boot).is_err());
}

#[test]
fn a_response_is_recorded_once_and_survives_restart() {
    let mut f = Fixture::new();
    let (job, generation) = transferring(&mut f, BootEpoch::INITIAL);
    let seen = Representation {
        length: Some(10),
        strong_etag: Some("\"v1\"".into()),
        last_modified: Some(NOW),
    };
    f.store
        .record_response(job, generation, &seen, Some("game.rar"), NOW)
        .unwrap();
    assert_eq!(f.summary(job).bytes_total, Some(10));
    assert!(matches!(
        f.events().last(),
        Some(EventKind::JobChanged { job: summary }) if summary.bytes_total == Some(10)
    ));
    // Later responses are compared with it, never recorded over it.
    let changed = Representation {
        strong_etag: Some("\"v2\"".into()),
        ..seen.clone()
    };
    assert!(matches!(
        f.store
            .record_response(job, generation, &changed, None, NOW),
        Err(StoreError::Staging(_))
    ));
    f.file(job, generation).write_all_at(b"0123", 0).unwrap();
    f.checkpoint(job, generation, &[(0, 4)]).unwrap();

    let mut f = f.reopen();
    let partial = f.store.latest_staging(job).unwrap().unwrap();
    assert_eq!(partial.representation, Some(seen));
    assert_eq!(partial.suggested_name.as_deref(), Some("game.rar"));
    assert_eq!((partial.durable_end, partial.contiguous), (4, true));
    assert!(!partial.is_complete());
    // Only the transferring generation records a response.
    f.store.recover_after_restart(NOW).unwrap();
    assert!(
        f.store
            .record_response(job, generation, &changed, None, NOW)
            .is_err()
    );
}

#[test]
fn a_partial_with_holes_is_not_contiguous() {
    let mut f = Fixture::new();
    let (job, generation) = transferring(&mut f, BootEpoch::INITIAL);
    f.file(job, generation)
        .write_all_at(b"0123456789", 0)
        .unwrap();
    f.checkpoint(job, generation, &[(0, 2), (6, 10)]).unwrap();
    let partial = f.store.latest_staging(job).unwrap().unwrap();
    assert_eq!((partial.durable_end, partial.contiguous), (10, false));
}

#[test]
fn admission_reads_only_free_slots_recovering_first() {
    let mut f = Fixture::new();
    let [first, second, third] = [f.job(), f.job(), f.job()];
    f.advance(second, JobStage::Resolving).unwrap();
    let mut f = f.reopen();
    f.store.recover_after_restart(NOW).unwrap();

    let ready = f.store.admissible(2, &[]).unwrap();
    let picked: Vec<_> = ready.iter().map(|a| (a.job, a.stage)).collect();
    assert_eq!(
        picked,
        vec![(second, JobStage::Recovering), (first, JobStage::Queued)]
    );
    assert_eq!(ready[1].url, "https://files.example/a.bin");
    assert_eq!(ready[0].generation, f.summary(second).generation);
    // Running jobs are skipped, and nothing is read without a free slot.
    let ready = f.store.admissible(2, &[second]).unwrap();
    let picked: Vec<_> = ready.iter().map(|a| a.job).collect();
    assert_eq!(picked, vec![first, third]);
    assert!(f.store.admissible(0, &[]).unwrap().is_empty());
}

#[test]
fn retention_prunes_events_and_idempotency_records() {
    let mut f = Fixture::new();
    let key = IdempotencyKey::generate();
    let first = f
        .run(key, submit_command(&["https://files.example/a.bin"], None))
        .unwrap();
    f.store.stage_pending(10, NOW).unwrap();
    f.job();
    let head = f.store.event_head().unwrap();
    assert!(head > 3);
    assert_eq!(f.store.retained_events().unwrap(), (0, head));

    // Records made before the cutoff go; newer ones stay.
    let later = UtcTimestamp::from_unix_millis(NOW.unix_millis() + 1);
    assert_eq!(f.store.prune(2, NOW).unwrap(), (head as usize - 2, 0));
    assert_eq!(f.store.retained_events().unwrap(), (head - 2, head));
    let kept = f.store.events_after(0, 10).unwrap();
    let positions: Vec<_> = kept.iter().map(|(p, _)| *p).collect();
    assert_eq!(positions, vec![head - 1, head]);
    assert_eq!(f.store.events_after(head - 1, 10).unwrap().len(), 1);
    assert_eq!(f.store.events_after(0, 1).unwrap().len(), 1);
    assert_eq!(f.store.prune(2, later).unwrap(), (0, 2));
    // Positions never move back and are never reused.
    assert_eq!(f.store.event_head().unwrap(), head);
    f.store.prune(0, later).unwrap();
    assert_eq!(f.store.retained_events().unwrap(), (head, head));

    // Past retention, the same key runs the command again.
    let again = f
        .run(key, submit_command(&["https://files.example/a.bin"], None))
        .unwrap();
    assert_ne!(again, first);
    assert_eq!(f.store.event_head().unwrap(), head + 1);
}
