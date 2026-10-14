//! Durable transfer progress (architecture brief section 7.6, ADR-16;
//! tests T23, T25, T27).
//!
//! The rule: **the durable range map never outruns durable file data.**
//!
//! * A checkpoint can only be built by [`SyncedRanges::after_sync`], which
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
//! What this module does not do yet: tie the synced file to the partial's
//! owned path and file identity, and validate a partial after a restart
//! before a new generation adopts it. Both arrive with the filesystem
//! interfaces.

use std::fs::File;
use std::io;
use std::ops::Range;

use rangemap::RangeSet;
use rusqlite::{OptionalExtension, params};
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::{JobError, JobStage};
use wayfetch_domain::version::Generation;

use super::{Store, StoreError, jobs, sql};

/// Byte ranges certified durable in a job generation's partial file.
#[derive(Debug)]
pub struct SyncedRanges {
    job: JobId,
    generation: Generation,
    ranges: Vec<Range<u64>>,
}

impl SyncedRanges {
    /// Synchronizes `file`'s data to stable storage, then certifies `ranges`
    /// of it for `job` at `generation`. The writer must have finished
    /// writing every byte of `ranges` before calling this.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by transfer workers, which arrive with the transfer interfaces"
        )
    )]
    pub fn after_sync(
        file: &File,
        job: JobId,
        generation: Generation,
        ranges: Vec<Range<u64>>,
    ) -> io::Result<Self> {
        file.sync_data()?;
        Ok(Self {
            job,
            generation,
            ranges,
        })
    }
}

impl Store {
    /// Commits a checkpoint and returns the partial's total durable bytes.
    /// Only now may that progress be reported as surviving a restart.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "called by transfer workers, which arrive with the transfer interfaces"
        )
    )]
    pub fn commit_checkpoint(&mut self, synced: SyncedRanges) -> Result<u64, StoreError> {
        let tx = self.write()?;
        let job = jobs::load(&tx, synced.job)?;
        if job.state.generation() != synced.generation {
            return Err(JobError::StaleGeneration {
                current: job.state.generation(),
                presented: synced.generation,
            }
            .into());
        }
        if job.state.stage() != JobStage::Transferring {
            return Err(StoreError::Checkpoint("job is not transferring"));
        }
        if synced.ranges.is_empty() {
            return Err(StoreError::Checkpoint("no ranges"));
        }
        let partial: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id, checkpoint_seq FROM partials WHERE job_id = ?1 AND generation = ?2",
                params![sql(synced.job)?, sql(synced.generation)?],
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
        let durable_sql =
            i64::try_from(durable).map_err(|_| StoreError::Corrupt("durable bytes"))?;
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
                    params![sql(synced.job)?, sql(synced.generation)?, durable_sql],
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
        tx.commit()?;
        Ok(durable)
    }
}

fn offset(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt("byte offset"))
}

fn offset_sql(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Corrupt("byte offset"))
}
