//! Unpacking downloaded archives: which archive downloads wait for their
//! other volumes, wait for their turn, are being unpacked, and how each
//! attempt ended (protocol 6.0, architecture brief section 9).
//!
//! Each attempt runs under a new generation; its result is recorded only
//! while the row is still at that attempt, so a cancelled or replaced
//! attempt cannot overwrite a later state. Every change announces the
//! archive's job with `job_changed`.

use std::str::FromStr;

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::job::JobStage;
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::{Generation, Revision};
use wayfetch_protocol::query::{ExtractionState, ExtractionSummary};
use wayfetch_protocol::{CommandResult, ErrorCode, ProtocolError};

use super::{Store, StoreError, jobs, key, renames, sql};
use crate::extract::{ArchiveName, Layout};
use crate::fs::{PublishedFile, RelativePath};

/// A job's stored extraction, from the job query's columns.
pub(super) fn decode(
    (state, output, problem): (Option<String>, Option<String>, Option<String>),
) -> Result<Option<ExtractionSummary>, StoreError> {
    let Some(state) = state else {
        return Ok(None);
    };
    Ok(Some(ExtractionSummary {
        state: ExtractionState::from_str(&state)
            .map_err(|_| StoreError::Corrupt("extraction state"))?,
        output,
        problem: problem.as_deref().map(DiagnosticText::new),
    }))
}

/// An attempt to unpack an archive, as the coordinator runs it.
#[derive(Debug, Clone)]
pub struct ExtractionStart {
    /// The archive's job.
    pub job: JobId,
    /// The attempt.
    pub generation: Generation,
    /// The root holding the archive.
    pub root: RootId,
    /// The archive's first volume as published.
    pub archive: PublishedFile,
    /// How it is unpacked.
    pub layout: Layout,
    /// What a folder of several entries is named.
    pub stem: String,
    /// The attempt's private folder.
    pub staging: String,
    /// An earlier attempt's private folder still to remove, if any.
    pub leftover: Option<String>,
}

/// How an attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionEnd {
    /// Unpacked to `output`.
    Complete(RelativePath),
    /// The archive needs a password, or the one given was wrong.
    NeedsPassword(Option<String>),
    /// It failed, and why.
    Failed(String),
}

/// The state of `job`'s extraction row and its generation, if any.
fn row(tx: &Transaction<'_>, job: JobId) -> Result<Option<(ExtractionState, i64)>, StoreError> {
    tx.query_row(
        "SELECT state, generation FROM extractions WHERE job_id = ?1",
        [sql(job)?],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()?
    .map(|(state, generation)| {
        Ok((
            ExtractionState::from_str(&state).map_err(|_| StoreError::Corrupt("extraction state"))?,
            generation,
        ))
    })
    .transpose()
}

/// Sets `job`'s extraction to `state`, adding the row if missing, clearing
/// its output and setting its problem, and announces the job.
fn set(
    tx: &Transaction<'_>,
    job: JobId,
    state: ExtractionState,
    problem: Option<&str>,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    let state: &'static str = state.into();
    tx.execute(
        "INSERT INTO extractions (job_id, generation, state, problem, updated_at) \
         VALUES (?1, 1, ?2, ?3, ?4) \
         ON CONFLICT (job_id) DO UPDATE SET state = ?2, output = NULL, problem = ?3, \
         updated_at = ?4",
        params![sql(job)?, state, problem, now.unix_millis()],
    )?;
    jobs::announce(tx, job, now)
}

/// The names of `archive`'s other volumes that have not finished
/// downloading in its root, by the jobs that download them.
fn unfinished_parts(
    tx: &Transaction<'_>,
    job: JobId,
    root: RootId,
    archive: &ArchiveName,
) -> Result<usize, StoreError> {
    let ids: Vec<i64> = tx
        .prepare(
            "SELECT id FROM jobs WHERE removed_at IS NULL AND stage != 'complete' \
             AND id != ?1 AND (root_id = ?2 OR root_id IS NULL)",
        )?
        .query_map(params![sql(job)?, sql(root)?], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut unfinished = 0;
    for id in ids {
        let other = jobs::load(tx, key(id)?)?;
        if other
            .name()
            .and_then(ArchiveName::parse)
            .is_some_and(|name| name.same_set(archive))
        {
            unfinished += 1;
        }
    }
    Ok(unfinished)
}

/// Queues `job`'s archive to be unpacked, or has it wait while other
/// volumes of it are still downloading.
fn queue(tx: &Transaction<'_>, job: JobId, now: UtcTimestamp) -> Result<(), StoreError> {
    let stored = jobs::load(tx, job)?;
    let (Some(archive), Some(root)) = (stored.archive(), stored.artifact_root()) else {
        return Err(StoreError::Corrupt("archive job"));
    };
    match unfinished_parts(tx, job, root, &archive)? {
        0 => set(tx, job, ExtractionState::Queued, None, now),
        n => {
            let problem = match n {
                1 => "Waiting for 1 more part to download.".to_owned(),
                n => format!("Waiting for {n} more parts to download."),
            };
            set(tx, job, ExtractionState::Waiting, Some(&problem), now)
        }
    }
}

impl Store {
    /// After `finished`'s download ended: an archive it completed is
    /// queued to be unpacked if `automatic`, and every archive waiting for
    /// other volumes is checked again. Answers whether anything changed.
    pub(crate) fn plan_extractions(
        &mut self,
        finished: JobId,
        automatic: bool,
        now: UtcTimestamp,
    ) -> Result<bool, StoreError> {
        let tx = self.write()?;
        let job = jobs::load(&tx, finished)?;
        let mut changed = false;
        if automatic
            && !job.removed
            && job.state.stage() == JobStage::Complete
            && job.archive().is_some_and(|a| a.first)
            && row(&tx, finished)?.is_none()
        {
            queue(&tx, finished, now)?;
            changed = true;
        }
        let waiting: Vec<i64> = tx
            .prepare("SELECT job_id FROM extractions WHERE state = 'waiting' ORDER BY job_id")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in waiting {
            let id: JobId = key(id)?;
            if id == finished && changed {
                continue;
            }
            queue(&tx, id, now)?;
            changed = true;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Takes the next queued archive to unpack, oldest first, under a new
    /// generation. An archive whose download was removed from the list or
    /// whose file is gone from the records fails instead.
    pub(crate) fn next_extraction(
        &mut self,
        now: UtcTimestamp,
    ) -> Result<Option<ExtractionStart>, StoreError> {
        let tx = self.write()?;
        loop {
            let next: Option<(i64, i64, Option<String>)> = tx
                .query_row(
                    "SELECT job_id, generation, staging FROM extractions WHERE state = 'queued' \
                     ORDER BY updated_at, job_id LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((id, generation, leftover)) = next else {
                tx.commit()?;
                return Ok(None);
            };
            let job: JobId = key(id)?;
            let stored = jobs::load(&tx, job)?;
            let published = renames::published(&tx, job)?;
            let (Some(archive), Some((_, root, file)), false) =
                (stored.archive(), published, stored.removed)
            else {
                set(
                    &tx,
                    job,
                    ExtractionState::Failed,
                    Some("The archive is no longer in the list of downloads."),
                    now,
                )?;
                continue;
            };
            let generation: Generation = key::<Generation>(generation)?
                .next()
                .map_err(|_| StoreError::Corrupt("extraction generation"))?;
            let staging = format!("{}-g{}", job.get(), generation.get());
            tx.execute(
                "UPDATE extractions SET state = 'extracting', generation = ?2, staging = ?3, \
                 output = NULL, problem = NULL, updated_at = ?4 WHERE job_id = ?1",
                params![id, sql(generation)?, staging, now.unix_millis()],
            )?;
            jobs::announce(&tx, job, now)?;
            tx.commit()?;
            return Ok(Some(ExtractionStart {
                job,
                generation,
                root,
                archive: file,
                layout: archive.layout,
                stem: archive.stem,
                staging,
                leftover,
            }));
        }
    }

    /// Records how attempt `generation` of `job`'s extraction ended, unless
    /// it was cancelled or replaced meanwhile. Its private folder, which
    /// the caller removed or published, is forgotten either way. Answers
    /// whether the end was recorded.
    pub(crate) fn finish_extraction(
        &mut self,
        job: JobId,
        generation: Generation,
        end: &ExtractionEnd,
        now: UtcTimestamp,
    ) -> Result<bool, StoreError> {
        let tx = self.write()?;
        let current = row(&tx, job)?;
        let ours = current
            .is_some_and(|(state, g)| state == ExtractionState::Extracting && g == sql(generation).unwrap_or(0));
        let staging = format!("{}-g{}", job.get(), generation.get());
        tx.execute(
            "UPDATE extractions SET staging = NULL WHERE job_id = ?1 AND staging = ?2",
            params![sql(job)?, staging],
        )?;
        if ours {
            let (state, output, problem): (ExtractionState, Option<&str>, Option<&str>) =
                match end {
                    ExtractionEnd::Complete(output) => {
                        (ExtractionState::Complete, Some(output.as_str()), None)
                    }
                    ExtractionEnd::NeedsPassword(problem) => {
                        (ExtractionState::NeedsPassword, None, problem.as_deref())
                    }
                    ExtractionEnd::Failed(problem) => {
                        (ExtractionState::Failed, None, Some(problem.as_str()))
                    }
                };
            let state: &'static str = state.into();
            let problem = problem.map(DiagnosticText::new);
            tx.execute(
                "UPDATE extractions SET state = ?2, output = ?3, problem = ?4, updated_at = ?5 \
                 WHERE job_id = ?1",
                params![
                    sql(job)?,
                    state,
                    output,
                    problem.as_ref().map(DiagnosticText::as_str),
                    now.unix_millis()
                ],
            )?;
            jobs::announce(&tx, job, now)?;
        }
        tx.commit()?;
        Ok(ours)
    }

    /// Runs once at startup: attempts the previous run left unpacking go
    /// back to the queue, and their private folders are removed before
    /// they start again.
    pub(crate) fn recover_extractions(&mut self, now: UtcTimestamp) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE extractions SET state = 'queued', updated_at = ?1 WHERE state = 'extracting'",
            [now.unix_millis()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Private folders of attempts that ended without removing them, such
    /// as by a crash while cancelling: their root, job, and name.
    pub(crate) fn stale_extractions(&mut self) -> Result<Vec<(RootId, JobId, String)>, StoreError> {
        let tx = self.read()?;
        let rows: Vec<(i64, String)> = tx
            .prepare(
                "SELECT job_id, staging FROM extractions \
                 WHERE staging IS NOT NULL AND state NOT IN ('extracting', 'queued')",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut stale = Vec::new();
        for (job, staging) in rows {
            let job: JobId = key(job)?;
            if let Some((_, root, _)) = renames::published(&tx, job)? {
                stale.push((root, job, staging));
            }
        }
        Ok(stale)
    }

    /// Forgets `job`'s private folder `staging` once it was removed.
    pub(crate) fn forget_extraction_staging(
        &mut self,
        job: JobId,
        staging: &str,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE extractions SET staging = NULL WHERE job_id = ?1 AND staging = ?2",
            params![sql(job)?, staging],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Refuses a change to `job`'s file while its archive is being unpacked.
pub(super) fn check_idle(tx: &Transaction<'_>, job: JobId) -> Result<(), StoreError> {
    if row(tx, job)?.is_some_and(|(state, _)| state == ExtractionState::Extracting) {
        return Err(StoreError::Busy(
            "the archive is being unpacked; try again once that finishes",
        ));
    }
    Ok(())
}

/// [`wayfetch_protocol::Command::Extract`]: queues a complete archive
/// download to be unpacked, again if it was before. One already queued or
/// being unpacked is left as it is.
pub(super) fn extract(
    tx: &Transaction<'_>,
    id: JobId,
    expected: Revision,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let job = jobs::load_listed(tx, id)?;
    job.state.check_complete(expected).map_err(StoreError::from)?;
    if !job.archive().is_some_and(|a| a.first) {
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            "the download is not an archive, or not its first part",
        ));
    }
    let running = row(tx, id)?.is_some_and(|(state, _)| {
        matches!(state, ExtractionState::Queued | ExtractionState::Extracting)
    });
    if !running {
        queue(tx, id, now)?;
    }
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: !running,
    })
}

/// [`wayfetch_protocol::Command::CancelExtraction`]: stops unpacking an
/// archive, or waiting to. The coordinator stops a running attempt.
pub(super) fn cancel(
    tx: &Transaction<'_>,
    id: JobId,
    expected: Revision,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let job = jobs::load_listed(tx, id)?;
    job.state.check_complete(expected).map_err(StoreError::from)?;
    let active = row(tx, id)?.is_some_and(|(state, _)| {
        matches!(
            state,
            ExtractionState::Waiting
                | ExtractionState::Queued
                | ExtractionState::Extracting
                | ExtractionState::NeedsPassword
        )
    });
    if active {
        set(tx, id, ExtractionState::Cancelled, None, now)?;
    }
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: active,
    })
}
