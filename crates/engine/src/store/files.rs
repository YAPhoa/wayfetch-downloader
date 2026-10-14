//! Staging-file records, adoption after restart, and publication intents
//! (sections 7.6 and 7.7, ADR-16, ADR-17; tests T24, T27, T28, T31).
//!
//! The store never touches files. The filesystem side ([`crate::fs`])
//! creates, validates, links, and reconciles them; these records say which
//! job generation owns which file, and what was intended when.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::job::{JobError, JobStage, Outcome};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{BootEpoch, Generation};

use super::{Store, StoreError, jobs, key, progress, sql};
use crate::fs::{FileIdentity, RelativePath, Removal, StagingRecord};
use crate::transfer::lease::Representation;
use crate::transfer::range::is_complete;

/// A job's newest staging file and what is durable in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    /// The file.
    pub record: StagingRecord,
    /// End of the durable coverage, 0 if none.
    pub durable_end: u64,
    /// Whether the coverage is exactly `0..durable_end`, so a sequential
    /// transfer can continue from its end.
    pub contiguous: bool,
    /// What the response that started the file reported, if recorded.
    pub representation: Option<Representation>,
    /// The file name that response suggested, if any.
    pub suggested_name: Option<String>,
}

impl Partial {
    /// Whether every byte of a representation of known length is durable.
    pub fn is_complete(&self) -> bool {
        self.contiguous
            && self.representation.as_ref().and_then(|r| r.length) == Some(self.durable_end)
    }
}

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

/// Marks the staging files of `job` from generations before `below` that a
/// generation may still reopen as abandoned: none can any more. Called when
/// a newer generation registers its own file, which is then the only one
/// the job can continue, and when the job goes back to the queue under a
/// new generation, which starts from zero (section 7.6).
pub(super) fn abandon_staging(
    tx: &Transaction<'_>,
    job: JobId,
    below: Generation,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE staging_files SET cleanup = 'pending' \
         WHERE job_id = ?1 AND generation < ?2 AND cleanup IS NULL",
        params![sql(job)?, sql(below)?],
    )?;
    Ok(())
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
        abandon_staging(&tx, file.job, file.generation)?;
        tx.commit()?;
        Ok(())
    }

    /// The newest staging file of `job` that a generation may still
    /// reopen, which is what a restart or a resumed job reopens.
    pub fn latest_staging(&mut self, job: JobId) -> Result<Option<Partial>, StoreError> {
        let tx = self.read()?;
        let Some((record, representation, suggested_name)) = tx
            .query_row(
                &format!(
                    "SELECT {STAGING_COLUMNS}, representation_known, representation_length, \
                     representation_etag, representation_modified, suggested_name, \
                     representation_resource FROM staging_files WHERE job_id = ?1 AND cleanup IS NULL \
                     ORDER BY generation DESC LIMIT 1"
                ),
                [sql(job)?],
                |row| {
                    let known: bool = row.get(7)?;
                    let representation = known.then(|| -> rusqlite::Result<_> {
                        Ok((
                            row.get::<_, Option<i64>>(8)?,
                            row.get::<_, Option<String>>(9)?,
                            row.get::<_, Option<i64>>(10)?,
                            row.get::<_, Option<String>>(12)?,
                        ))
                    });
                    Ok((
                        read_staging(row)?,
                        representation.transpose()?,
                        row.get::<_, Option<String>>(11)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let record = record.decode()?;
        let representation = representation
            .map(|(length, etag, modified, resource)| {
                Ok::<_, StoreError>(Representation {
                    length: length
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| StoreError::Corrupt("representation length"))?,
                    strong_etag: etag,
                    last_modified: modified.map(UtcTimestamp::from_unix_millis),
                    resource,
                })
            })
            .transpose()?;
        let (_, coverage) = progress::load_coverage(&tx, job, record.generation)?;
        let durable_end = coverage.iter().next_back().map_or(0, |r| r.end);
        Ok(Some(Partial {
            contiguous: is_complete(coverage.iter(), durable_end),
            record,
            durable_end,
            representation,
            suggested_name,
        }))
    }

    /// Records what the response that started the job generation's staging
    /// file reported (its representation and suggested file name), and the
    /// job's expected size. Once recorded it does not change: a later
    /// response must report the same representation for its bytes to join
    /// the file.
    pub fn record_response(
        &mut self,
        job: JobId,
        generation: Generation,
        representation: &Representation,
        suggested_name: Option<&str>,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        job_at(
            &tx,
            job,
            generation,
            &[JobStage::Transferring],
            "recording a response needs a transferring job",
        )?;
        let length = representation
            .length
            .map(i64::try_from)
            .transpose()
            .map_err(|_| StoreError::Corrupt("representation length"))?;
        let updated = tx.execute(
            "UPDATE staging_files SET representation_known = 1, representation_length = ?1, \
             representation_etag = ?2, representation_modified = ?3, suggested_name = ?6, \
             representation_resource = ?7 WHERE job_id = ?4 AND generation = ?5 AND representation_known = 0",
            params![
                length,
                representation.strong_etag,
                representation.last_modified.map(UtcTimestamp::unix_millis),
                sql(job)?,
                sql(generation)?,
                suggested_name,
                representation.resource,
            ],
        )?;
        if updated != 1 {
            return Err(StoreError::Staging(
                "no staging file without a recorded representation",
            ));
        }
        tx.execute(
            "UPDATE jobs SET bytes_total = ?1, updated_at = ?2 WHERE id = ?3",
            params![length, now.unix_millis(), sql(job)?],
        )?;
        jobs::announce(&tx, job, now)?;
        tx.commit()?;
        Ok(())
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
                 AND cleanup IS NULL ORDER BY generation DESC LIMIT 1",
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
        job_at(
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
        // Loaded after the publication is recorded, so the completion event
        // carries the published file.
        let mut stored = jobs::load(&tx, job)?;
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

    /// Staging files in `root` that no generation can reopen and cleanup
    /// has not yet removed, at most `limit`. A file an unfinished
    /// publication names is never among them.
    pub fn abandoned_staging(
        &mut self,
        root: RootId,
        limit: usize,
    ) -> Result<Vec<StagingRecord>, StoreError> {
        let tx = self.read()?;
        let mut statement = tx.prepare(&format!(
            "SELECT {STAGING_COLUMNS} FROM staging_files s \
             WHERE root_id = ?1 AND cleanup = 'pending' AND NOT EXISTS ( \
                 SELECT 1 FROM publications p WHERE p.state = 'intended' \
                 AND p.root_id = s.root_id AND p.staging_name = s.name) \
             ORDER BY job_id, generation LIMIT ?2"
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let raw = statement
            .query_map(params![sql(root)?, limit], read_staging)?
            .collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(RawStaging::decode).collect()
    }

    /// Records what cleanup found for the abandoned staging file `record`.
    /// A file that was removed or is gone takes its record and recorded
    /// coverage with it; a name holding another file keeps the record,
    /// marked so that cleanup leaves the name alone from now on.
    pub fn finish_staging_cleanup(
        &mut self,
        record: &StagingRecord,
        found: Removal,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        let key = params![
            sql(record.job)?,
            sql(record.generation)?,
            sql(record.root)?,
            record.name,
            record.identity.device.cast_signed(),
            record.identity.inode.cast_signed(),
        ];
        let matched = "job_id = ?1 AND generation = ?2 AND root_id = ?3 AND name = ?4 \
             AND file_device = ?5 AND file_inode = ?6 AND cleanup = 'pending'";
        let changed = match found {
            Removal::Removed | Removal::Missing => {
                tx.execute(&format!("DELETE FROM staging_files WHERE {matched}"), key)?
            }
            Removal::Foreign => tx.execute(
                &format!("UPDATE staging_files SET cleanup = 'foreign' WHERE {matched}"),
                key,
            )?,
        };
        if changed != 1 {
            return Err(StoreError::Staging("no abandoned staging file to clean up"));
        }
        if found != Removal::Foreign {
            // Its ranges cascade.
            tx.execute(
                "DELETE FROM partials WHERE job_id = ?1 AND generation = ?2",
                params![sql(record.job)?, sql(record.generation)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
