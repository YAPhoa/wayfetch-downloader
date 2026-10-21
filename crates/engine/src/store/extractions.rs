//! Unpacking downloaded archives: which archive downloads wait for their
//! other volumes, wait for their turn, are being unpacked, and how each
//! attempt ended (architecture brief section 9).
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

use super::{OrCorrupt, Store, StoreError, jobs, key, renames, sql};
use crate::extract::{ArchiveName, Layout};
use crate::fs::{PublishedFile, RelativePath};

/// A job's stored extraction, from the job query's columns.
pub(super) fn decode(
    (state, output, problem, generation): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    ),
) -> Result<Option<ExtractionSummary>, StoreError> {
    let Some(state) = state else {
        return Ok(None);
    };
    Ok(Some(ExtractionSummary {
        state: ExtractionState::from_str(&state).or_corrupt("extraction state")?,
        generation: generation.map(key).transpose()?,
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
    /// Its other volumes downloaded so far, beside it.
    pub volumes: Vec<PublishedFile>,
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
            ExtractionState::from_str(&state).or_corrupt("extraction state")?,
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

/// Starts a new run of `job`'s extraction: a generation no earlier
/// request names, so a late or repeated stop or unpack cannot act on it.
fn begin(tx: &Transaction<'_>, job: JobId, now: UtcTimestamp) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO extractions (job_id, generation, state, updated_at) \
         VALUES (?1, 1, 'queued', ?2) \
         ON CONFLICT (job_id) DO UPDATE SET generation = generation + 1",
        params![sql(job)?, now.unix_millis()],
    )?;
    queue(tx, job, now)
}

/// Refuses a request naming `expected` as `job`'s unpacking run unless
/// that is the current one.
fn check_run(
    current: Option<(ExtractionState, i64)>,
    expected: Option<Generation>,
) -> Result<(), ProtocolError> {
    let expected = expected.map(sql).transpose()?;
    if current.map(|(_, generation)| generation) != expected {
        return Err(ProtocolError::new(
            ErrorCode::RevisionConflict,
            "the archive's unpacking changed since it was shown",
        ));
    }
    Ok(())
}

/// The other volumes of `archive`, the first volume `job` published at
/// `file` in `root`, that finished downloading beside it.
fn volumes(
    tx: &Transaction<'_>,
    job: JobId,
    root: RootId,
    archive: &ArchiveName,
    file: &PublishedFile,
) -> Result<Vec<PublishedFile>, StoreError> {
    let folder = file.path.split_last().0;
    let ids: Vec<(i64, String)> = tx
        .prepare(
            "SELECT a.job_id, a.target_path FROM publications a JOIN jobs j ON j.id = a.job_id \
             WHERE a.state = 'published' AND a.root_id = ?2 AND a.job_id != ?1 \
             AND j.removed_at IS NULL AND j.stage = 'complete'",
        )?
        .query_map(params![sql(job)?, sql(root)?], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut volumes = Vec::new();
    for (id, path) in ids {
        let (parent, name) = match path.rsplit_once('/') {
            Some((parent, name)) => (Some(parent), name),
            None => (None, path.as_str()),
        };
        if parent != folder || !ArchiveName::parse(name).is_some_and(|name| name.same_set(archive))
        {
            continue;
        }
        if let Some((_, _, file)) = renames::published(tx, key(id)?)? {
            volumes.push(file);
        }
    }
    Ok(volumes)
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
            begin(&tx, finished, now)?;
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

    /// Takes the next queued archive to unpack, oldest first, under its
    /// run's generation, with the volumes beside it. An archive whose
    /// download was removed from the list or whose file is gone from the
    /// records fails instead.
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
            let generation: Generation = key(generation)?;
            let volumes = volumes(&tx, job, root, &archive, &file)?;
            let staging = format!("{}-g{}", job.get(), generation.get());
            tx.execute(
                "UPDATE extractions SET state = 'extracting', staging = ?2, \
                 output = NULL, problem = NULL, updated_at = ?3 WHERE job_id = ?1",
                params![id, staging, now.unix_millis()],
            )?;
            jobs::announce(&tx, job, now)?;
            tx.commit()?;
            return Ok(Some(ExtractionStart {
                job,
                generation,
                root,
                archive: file,
                volumes,
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
        let ours = current.is_some_and(|(state, g)| {
            state == ExtractionState::Extracting && g == sql(generation).unwrap_or(0)
        });
        let staging = format!("{}-g{}", job.get(), generation.get());
        tx.execute(
            "UPDATE extractions SET staging = NULL WHERE job_id = ?1 AND staging = ?2",
            params![sql(job)?, staging],
        )?;
        if ours {
            let (state, output, problem): (ExtractionState, Option<&str>, Option<&str>) = match end
            {
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

/// Refuses a change to `job`'s file while it is unpacked, as an archive
/// or one of the volumes of one.
pub(super) fn check_idle(tx: &Transaction<'_>, job: JobId) -> Result<(), StoreError> {
    let unpacking: Vec<i64> = tx
        .prepare("SELECT job_id FROM extractions WHERE state = 'extracting'")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let own = renames::published(tx, job)?;
    for id in unpacking {
        let id: JobId = key(id)?;
        let mut busy = id == job;
        if let (false, Some((_, own_root, own)), Some(archive), Some((_, root, file))) = (
            busy,
            &own,
            jobs::load(tx, id)?.archive(),
            renames::published(tx, id)?,
        ) {
            busy = *own_root == root
                && volumes(tx, id, root, &archive, &file)?
                    .iter()
                    .any(|volume| volume.path == own.path);
        }
        if busy {
            return Err(StoreError::Busy(
                "the archive is being unpacked; try again once that finishes",
            ));
        }
    }
    Ok(())
}

/// [`wayfetch_protocol::Command::Extract`]: queues a complete archive
/// download to be unpacked under a new run, again if it was before, if
/// `run` is the run the client saw. One already queued or being unpacked
/// is left as it is.
pub(super) fn extract(
    tx: &Transaction<'_>,
    id: JobId,
    expected: Revision,
    run: Option<Generation>,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let job = jobs::load_listed(tx, id)?;
    job.state
        .check_complete(expected)
        .map_err(StoreError::from)?;
    if !job.archive().is_some_and(|a| a.first) {
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            "the download is not an archive, or not its first part",
        ));
    }
    let current = row(tx, id)?;
    check_run(current, run)?;
    let running = current.is_some_and(|(state, _)| {
        matches!(state, ExtractionState::Queued | ExtractionState::Extracting)
    });
    if !running {
        begin(tx, id, now)?;
    }
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: !running,
    })
}

/// [`wayfetch_protocol::Command::CancelExtraction`]: stops run `run` of
/// unpacking an archive, or waiting to, and never a later one. The
/// coordinator stops a running attempt.
pub(super) fn cancel(
    tx: &Transaction<'_>,
    id: JobId,
    expected: Revision,
    run: Generation,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let job = jobs::load_listed(tx, id)?;
    job.state
        .check_complete(expected)
        .map_err(StoreError::from)?;
    let current = row(tx, id)?;
    check_run(current, Some(run))?;
    let active = current.is_some_and(|(state, _)| {
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
