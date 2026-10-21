//! Durable transfer progress (architecture brief section 7.6, ADR-16;
//! tests T23, T25, T27).
//!
//! The rule: **the durable range map never outruns durable file data.**
//!
//! * A checkpoint can only be built by [`StagingFile::sync_ranges`], which
//!   synchronizes the partial file first. A sync error is a failure, never
//!   progress. The database commits afterwards, so a crash in between
//!   leaves bytes on disk that the map does not trust, never the reverse.
//! * Ranges are half-open byte intervals of the partial owned by one job
//!   generation. A checkpoint from any other generation is rejected, so a
//!   late writer from a cancelled or pre-restart run cannot claim coverage.
//! * New ranges must not overlap ranges already durable or each other: each
//!   byte is acknowledged once, by the writer that owns it. Adjacent ranges
//!   are merged when stored.
//! * Coverage is the set of committed ranges. File length is never used as
//!   coverage, so a sparse or preallocated file with holes is not mistaken
//!   for a complete one.
//!
//! * The synced file must be the staging file registered for that job
//!   generation, matched by root, name, and file identity, so bytes written
//!   to any other file cannot be certified.

use rangemap::RangeSet;
use rusqlite::Transaction;
use rusqlite::{OptionalExtension, params};
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::{JobError, JobStage};
use wayfetch_domain::version::Generation;

use super::{OrCorrupt, Store, StoreError, files, jobs, sql};
#[cfg(doc)]
use crate::fs::StagingFile;
use crate::fs::SyncedRanges;

impl Store {
    /// Commits a checkpoint and returns the partial's total durable bytes.
    /// Only now may that progress be reported as surviving a restart.
    pub fn commit_checkpoint(&mut self, synced: SyncedRanges) -> Result<u64, StoreError> {
        let tx = self.write()?;
        let (job_id, generation) = (synced.file.job, synced.file.generation);
        let job = jobs::load(&tx, job_id)?;
        if job.state.generation() != generation {
            return Err(JobError::StaleGeneration {
                current: job.state.generation(),
                presented: generation,
            }
            .into());
        }
        if job.state.stage() != JobStage::Transferring {
            return Err(StoreError::Checkpoint("job is not transferring"));
        }
        if files::staging(&tx, job_id, generation)?.as_ref() != Some(&synced.file) {
            return Err(StoreError::Checkpoint(
                "file is not the generation's registered staging file",
            ));
        }
        if synced.ranges.is_empty() {
            return Err(StoreError::Checkpoint("no ranges"));
        }
        let (partial, mut coverage) = load_coverage(&tx, job_id, generation)?;
        for range in synced.ranges {
            if range.is_empty() || i64::try_from(range.end).is_err() {
                return Err(StoreError::Checkpoint("range is empty or out of bounds"));
            }
            if coverage.overlaps(&range) {
                return Err(StoreError::Checkpoint("range overlaps durable coverage"));
            }
            coverage.insert(range);
        }
        // Disjoint ranges below i64::MAX cannot sum past u64::MAX.
        let durable: u64 = coverage.iter().map(|r| r.end - r.start).sum();
        let durable_sql = i64::try_from(durable).or_corrupt("durable bytes")?;
        let partial = match partial {
            Some((id, seq)) => {
                tx.execute(
                    "UPDATE partials SET checkpoint_seq = ?1, durable_bytes = ?2 WHERE id = ?3",
                    params![seq + 1, durable_sql, id],
                )?;
                tx.execute("DELETE FROM durable_ranges WHERE partial_id = ?1", [id])?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO partials (job_id, generation, checkpoint_seq, durable_bytes) \
                     VALUES (?1, ?2, 1, ?3)",
                    params![sql(job_id)?, sql(generation)?, durable_sql],
                )?;
                tx.last_insert_rowid()
            }
        };
        let mut insert = tx.prepare(
            "INSERT INTO durable_ranges (partial_id, start_inclusive, end_exclusive) \
             VALUES (?1, ?2, ?3)",
        )?;
        for range in coverage.iter() {
            insert.execute(params![
                partial,
                offset_sql(range.start)?,
                offset_sql(range.end)?
            ])?;
        }
        drop(insert);
        crash_point!("checkpoint-before-commit");
        tx.commit()?;
        crash_point!("checkpoint-after-commit");
        Ok(durable)
    }
}

/// A stored partial's row ID and checkpoint sequence.
type PartialRow = (i64, i64);

/// The partial of `job` at `generation`, if any, and its durable coverage,
/// checked for consistency.
pub(super) fn load_coverage(
    tx: &Transaction<'_>,
    job: JobId,
    generation: Generation,
) -> Result<(Option<PartialRow>, RangeSet<u64>), StoreError> {
    let partial: Option<PartialRow> = tx
        .query_row(
            "SELECT id, checkpoint_seq FROM partials WHERE job_id = ?1 AND generation = ?2",
            params![sql(job)?, sql(generation)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let mut coverage = RangeSet::new();
    if let Some((partial, _)) = partial {
        let stored = tx
            .prepare(
                "SELECT start_inclusive, end_exclusive FROM durable_ranges \
                 WHERE partial_id = ?1",
            )?
            .query_map([partial], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (start, end) in stored {
            let range = offset(start)?..offset(end)?;
            if range.is_empty() || coverage.overlaps(&range) {
                return Err(StoreError::Corrupt("durable ranges"));
            }
            coverage.insert(range);
        }
    }
    Ok((partial, coverage))
}

fn offset(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).or_corrupt("byte offset")
}

fn offset_sql(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).or_corrupt("byte offset")
}
