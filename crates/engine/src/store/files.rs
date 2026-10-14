//! Staging-file records, adoption after restart, and publication intents
//! (sections 7.6 and 7.7, ADR-16, ADR-17; tests T24, T27, T28, T31).
//!
//! The store never touches files. The filesystem side ([`crate::fs`])
//! creates, validates, links, and reconciles them; these records say which
//! job generation owns which file, and what was intended when.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called by the transfer coordinator, which arrives with the first acceptance slice"
    )
)]

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::{JobError, JobStage, Outcome};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{BootEpoch, Generation};

use super::{Store, StoreError, jobs, key, progress, sql};
use crate::fs::{FileIdentity, RelativePath, StagingRecord};
use crate::transfer::range::is_complete;

/// A recorded publication that has not completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationIntent {
    /// The staging file being published, as it was when the intent was
    /// recorded.
    pub staging: StagingRecord,
    /// The reserved target.
    pub target: RelativePath,
    /// Expected size of the artifact.
    pub size: u64,
}

const STAGING_COLUMNS: &str =
    "job_id, generation, root_id, name, file_device, file_inode, file_birth_ns";

fn read_staging(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawStaging> {
    Ok(RawStaging {
        job: row.get(0)?,
        generation: row.get(1)?,
        root: row.get(2)?,
        name: row.get(3)?,
        device: row.get(4)?,
        inode: row.get(5)?,
        birth_ns: row.get(6)?,
    })
}

struct RawStaging {
    job: i64,
    generation: i64,
    root: i64,
    name: String,
    device: i64,
    inode: i64,
    birth_ns: Option<i64>,
}

impl RawStaging {
    fn decode(self) -> Result<StagingRecord, StoreError> {
        Ok(StagingRecord {
            root: key(self.root)?,
            job: key(self.job)?,
            generation: key(self.generation)?,
            name: self.name,
            identity: FileIdentity {
                device: self.device.cast_unsigned(),
                inode: self.inode.cast_unsigned(),
                birth_ns: self.birth_ns,
            },
        })
    }
}

/// The staging record of `job` at `generation`, if any.
pub(super) fn staging(
    tx: &Transaction<'_>,
    job: JobId,
    generation: Generation,
) -> Result<Option<StagingRecord>, StoreError> {
    tx.query_row(
        &format!(
            "SELECT {STAGING_COLUMNS} FROM staging_files WHERE job_id = ?1 AND generation = ?2"
        ),
        params![sql(job)?, sql(generation)?],
        read_staging,
    )
    .optional()?
    .map(RawStaging::decode)
    .transpose()
}

/// Loads `id` and checks it is at `generation` in one of `stages`.
fn job_at(
    tx: &Transaction<'_>,
    id: JobId,
    generation: Generation,
    stages: &[JobStage],
    what: &'static str,
) -> Result<jobs::StoredJob, StoreError> {
    let job = jobs::load(tx, id)?;
    if job.state.generation() != generation {
        return Err(JobError::StaleGeneration {
            current: job.state.generation(),
            presented: generation,
        }
        .into());
    }
    if !stages.contains(&job.state.stage()) {
        return Err(StoreError::Staging(what));
    }
    Ok(job)
}

impl Store {
    /// Records a newly created staging file as owned by its job generation,
    /// created in boot `epoch`. The job must be transferring at that
    /// generation.
    pub fn register_staging(
        &mut self,
        file: &StagingRecord,
        epoch: BootEpoch,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        job_at(
            &tx,
            file.job,
            file.generation,
            &[JobStage::Transferring],
            "staging needs a transferring job",
        )?;
        tx.execute(
            "INSERT INTO staging_files (job_id, generation, root_id, name, file_device, \
             file_inode, file_birth_ns, boot_epoch, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                sql(file.job)?,
                sql(file.generation)?,
                sql(file.root)?,
                file.name,
                file.identity.device.cast_signed(),
                file.identity.inode.cast_signed(),
                file.identity.birth_ns,
                sql(epoch)?,
                now.unix_millis(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The newest staging record of `job` and the end of its durable
    /// coverage (0 if none), which is what a restart reopens it with.
    pub fn latest_staging(
        &mut self,
        job: JobId,
    ) -> Result<Option<(StagingRecord, u64)>, StoreError> {
        let tx = self.read()?;
        let Some(record) = tx
            .query_row(
                &format!(
                    "SELECT {STAGING_COLUMNS} FROM staging_files WHERE job_id = ?1 \
                     ORDER BY generation DESC LIMIT 1"
                ),
                [sql(job)?],
                read_staging,
            )
            .optional()?
            .map(RawStaging::decode)
            .transpose()?
        else {
            return Ok(None);
        };
        let (_, coverage) = progress::load_coverage(&tx, job, record.generation)?;
        let end = coverage.iter().next_back().map_or(0, |r| r.end);
        Ok(Some((record, end)))
    }

    /// Hands the newest staging file of a recovering job, with its durable
    /// coverage, to the job's current generation. The caller has reopened
    /// and validated the file first ([`crate::fs::Root::reopen_staging`]).
    ///
    /// Only a file from an earlier engine start (`epoch`) can be adopted:
    /// no writer of this process can still hold it, so the new generation
    /// is its only writer (section 7.6).
    pub fn adopt_staging(
        &mut self,
        job: JobId,
        generation: Generation,
        epoch: BootEpoch,
    ) -> Result<StagingRecord, StoreError> {
        let tx = self.write()?;
        job_at(
            &tx,
            job,
            generation,
            &[JobStage::Recovering],
            "adoption needs a recovering job",
        )?;
        let (previous, created): (i64, i64) = tx
            .query_row(
                "SELECT generation, boot_epoch FROM staging_files WHERE job_id = ?1 \
                 ORDER BY generation DESC LIMIT 1",
                [sql(job)?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound("staging file"))?;
        if previous >= sql(generation)? || created >= sql(epoch)? {
            return Err(StoreError::Staging(
                "only a file from an earlier generation and engine start can be adopted",
            ));
        }
        for table in ["staging_files", "partials"] {
            tx.execute(
                &format!(
                    "UPDATE {table} SET generation = ?1 WHERE job_id = ?2 AND generation = ?3"
                ),
                params![sql(generation)?, sql(job)?, previous],
            )?;
        }
        tx.execute(
            "UPDATE staging_files SET boot_epoch = ?1 WHERE job_id = ?2 AND generation = ?3",
            params![sql(epoch)?, sql(job)?, sql(generation)?],
        )?;
        let record = staging(&tx, job, generation)?.ok_or(StoreError::Corrupt("staging file"))?;
        tx.commit()?;
        Ok(record)
    }

    /// Records the intent to publish the job generation's staging file at
    /// `target` as an artifact of `size` bytes, reserving the target. The
    /// job must be publishing, and durable coverage must be exactly
    /// `0..size`: file length is never accepted in place of coverage (T25).
    /// A target whose case-folded name is already reserved in the root is
    /// refused with [`StoreError::TargetReserved`].
    pub fn begin_publication(
        &mut self,
        job: JobId,
        generation: Generation,
        target: &RelativePath,
        size: u64,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        job_at(
            &tx,
            job,
            generation,
            &[JobStage::Publishing],
            "publication needs a publishing job",
        )?;
        let file = staging(&tx, job, generation)?.ok_or(StoreError::NotFound("staging file"))?;
        let (_, coverage) = progress::load_coverage(&tx, job, generation)?;
        if !is_complete(coverage.iter(), size) {
            return Err(StoreError::Staging(
                "durable coverage is not the whole artifact",
            ));
        }
        let size_sql = i64::try_from(size).map_err(|_| StoreError::Corrupt("size"))?;
        let reserved: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM publications WHERE root_id = ?1 AND target_key = ?2)",
            params![sql(file.root)?, target.collision_key()],
            |row| row.get(0),
        )?;
        if reserved {
            return Err(StoreError::TargetReserved(target.clone()));
        }
        tx.execute(
            "INSERT INTO publications (job_id, generation, root_id, staging_name, file_device, \
             file_inode, file_birth_ns, target_path, target_key, size_bytes, state, \
             created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'intended', ?11)",
            params![
                sql(job)?,
                sql(generation)?,
                sql(file.root)?,
                file.name,
                file.identity.device.cast_signed(),
                file.identity.inode.cast_signed(),
                file.identity.birth_ns,
                target.as_str(),
                target.collision_key(),
                size_sql,
                now.unix_millis(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The job's unfinished publication, which a restart reconciles with
    /// [`crate::fs::Root::reconcile_publication`].
    pub fn pending_publication(
        &mut self,
        job: JobId,
    ) -> Result<Option<PublicationIntent>, StoreError> {
        let tx = self.read()?;
        let row = tx
            .query_row(
                "SELECT job_id, generation, root_id, staging_name, file_device, file_inode, \
                 file_birth_ns, target_path, size_bytes FROM publications \
                 WHERE job_id = ?1 AND state = 'intended'",
                [sql(job)?],
                |row| {
                    Ok((
                        read_staging(row)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(staging, target, size)| {
            Ok(PublicationIntent {
                staging: staging.decode()?,
                target: RelativePath::new(&target)
                    .map_err(|_| StoreError::Corrupt("publication target"))?,
                size: u64::try_from(size).map_err(|_| StoreError::Corrupt("size"))?,
            })
        })
        .transpose()
    }

    /// Completes the job's pending publication, and the job, in one
    /// transaction. Call it only after [`crate::fs::Root::publish`] or
    /// [`crate::fs::Root::reconcile_publication`] confirmed the intent's
    /// file at its target with the intent's size. The job is
    /// publishing, or recovering if a restart interrupted publication.
    pub fn complete_publication(
        &mut self,
        job: JobId,
        generation: Generation,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        let mut stored = job_at(
            &tx,
            job,
            generation,
            &[JobStage::Publishing, JobStage::Recovering],
            "completion needs a publishing or recovering job",
        )?;
        let (intent, root, name): (i64, i64, String) = tx
            .query_row(
                "SELECT id, root_id, staging_name FROM publications \
                 WHERE job_id = ?1 AND state = 'intended'",
                [sql(job)?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound("publication intent"))?;
        tx.execute(
            "UPDATE publications SET state = 'published', completed_at = ?1 WHERE id = ?2",
            params![now.unix_millis(), intent],
        )?;
        // The published file no longer has a staging name. Records of older
        // generations' files stay for their cleanup.
        tx.execute(
            "DELETE FROM staging_files WHERE root_id = ?1 AND name = ?2",
            params![root, name],
        )?;
        let previous = stored.state;
        if stored.state.advance(JobStage::Complete, generation)? == Outcome::Changed {
            jobs::save(&tx, &mut stored, &previous, None, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the job's pending publication, releasing its reservation, when
    /// reconciliation found another file at the target or the staged file
    /// lost. The files themselves are left alone.
    pub fn abandon_publication(
        &mut self,
        job: JobId,
        generation: Generation,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        job_at(
            &tx,
            job,
            generation,
            &[JobStage::Publishing, JobStage::Recovering],
            "abandoning needs a publishing or recovering job",
        )?;
        tx.execute(
            "DELETE FROM publications WHERE job_id = ?1 AND state = 'intended'",
            [sql(job)?],
        )?;
        tx.commit()?;
        Ok(())
    }
}
