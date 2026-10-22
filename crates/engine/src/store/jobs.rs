//! Job rows: staging, generation-checked transitions, restart recovery, and
//! read queries.

use std::str::FromStr;

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};
use strum::VariantArray;
use wayfetch_domain::failure::{Failure, FailureCode};
use wayfetch_domain::id::{JobId, PackageId, RootId};
use wayfetch_domain::job::{DesiredState, JobStage, JobState, Outcome};
use wayfetch_domain::name::{DiagnosticText, HostName, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::event::{EventCursor, EventKind};
use wayfetch_protocol::media::{JobMedia, MediaChoice};
use wayfetch_protocol::query::{
    ArtifactSummary, ExtractionState, ExtractionSummary, FailureSummary, FileCheck, JobCounts,
    JobFilter, JobRoute, JobSummary, Page, Query, QueryResult,
};
use wayfetch_protocol::{InstanceId, ProtocolError};

use super::{OrCorrupt, Store, StoreError, files, key, links, media, operations, outbox, sql};
use crate::transfer::transport::Reach;

/// Columns read by [`RawJob::read`]. `bytes_done` is the durable progress of
/// the job's newest partial; the next three describe its published file,
/// then come the submitted link, the job's destination root, whether the
/// user removed it from lists, and why its latest attempt restarted from
/// zero, and the provider route it was started through; then its media,
/// unpacking, and the check of its file that matters most, with whether
/// the file is being replaced (see [`super::checks::decode`]).
const JOB_COLUMNS: &str = "j.id, j.package_id, j.desired_state, j.stage, j.generation, \
     j.revision, j.bytes_total, j.failure_code, j.failure_retry_at, j.failure_message, \
     COALESCE((SELECT p.durable_bytes FROM partials p WHERE p.job_id = j.id \
               ORDER BY p.generation DESC LIMIT 1), 0), \
     a.root_id, a.target_path, a.size_bytes, s.input_url, j.root_id, j.removed_at IS NOT NULL, \
     j.restart_reason, j.route_service, j.route_account, j.suggested_name, \
     j.media_service, j.media_choice, x.state, x.output, x.problem, x.generation, \
     (SELECT v.state || char(31) || v.algorithm || char(31) || COALESCE(v.source_name, '') \
      || char(31) || COALESCE(v.problem, '') FROM verifications v \
      WHERE v.publication_id = a.id ORDER BY CASE v.state WHEN 'changed' THEN 0 \
      WHEN 'conflict' THEN 1 WHEN 'failed' THEN 2 WHEN 'pending' THEN 3 ELSE 4 END, \
      v.source_publication_id LIMIT 1), \
     EXISTS (SELECT 1 FROM replacements r WHERE r.job_id = j.id)";

/// The tables [`JOB_COLUMNS`] reads. A job publishes at most one file and
/// has exactly one source link.
const JOB_FROM: &str = "jobs j JOIN source_links s ON s.id = j.source_link_id \
     LEFT JOIN publications a ON a.job_id = j.id AND a.state = 'published' \
     LEFT JOIN extractions x ON x.job_id = j.id";

/// A job the coordinator can start work on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The job.
    pub job: JobId,
    /// Queued or recovering.
    pub stage: JobStage,
    /// Its generation.
    pub generation: Generation,
    /// What the user asked for.
    pub desired: DesiredState,
    /// The submitted input.
    pub url: String,
    /// Where it is saved, if decided yet.
    pub root: Option<RootId>,
    /// The provider account it downloads through; `None` for a direct
    /// download.
    pub route: Option<JobRoute>,
    /// The media service it is made by, and what it is made as.
    pub media: Option<JobMedia>,
    /// What its requests may reach.
    pub reach: Reach,
}

/// What a new download of a media link is made by and as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct NewMedia {
    /// The media service.
    pub(super) service: ProviderId,
    /// What it is made as.
    pub(super) choice: MediaChoice,
    /// The title the link's check found, for the name shown until the
    /// file is named.
    pub(super) title: Option<String>,
}

/// Stages that are parked with a failure explaining why.
fn parked(stage: JobStage) -> bool {
    stage == JobStage::Failed
}

/// A stored job, decoded and validated.
pub(super) struct StoredJob {
    pub(super) id: JobId,
    pub(super) state: JobState,
    package: PackageId,
    bytes_total: Option<u64>,
    bytes_done: u64,
    pub(super) failure: Option<FailureSummary>,
    artifact: Option<ArtifactSummary>,
    original_host: HostName,
    destination: Option<RootId>,
    /// Removed from lists and counts by the user.
    pub(super) removed: bool,
    /// Why the latest attempt started from zero.
    restart_reason: Option<DiagnosticText>,
    /// The provider account it downloads through.
    route: Option<JobRoute>,
    name: Option<String>,
    media: Option<JobMedia>,
    extraction: Option<ExtractionSummary>,
    check: Option<FileCheck>,
}

impl StoredJob {
    /// Its package.
    pub(super) fn package(&self) -> PackageId {
        self.package
    }

    /// The name it was given before any number was added to make it
    /// unique: the source's or provider's suggestion, else the link's.
    pub(super) fn intended_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Where its published file is in its root, once complete.
    pub(super) fn artifact_path(&self) -> Option<&str> {
        self.artifact.as_ref().map(|a| a.path.as_str())
    }

    /// The published file's name as an archive, if it is one.
    pub(super) fn archive(&self) -> Option<crate::extract::ArchiveName> {
        let path = &self.artifact.as_ref()?.path;
        let name = path.rsplit('/').next()?;
        crate::extract::ArchiveName::parse(name)
    }

    /// The root holding the published file, once complete.
    pub(super) fn artifact_root(&self) -> Option<RootId> {
        self.artifact.as_ref().map(|a| a.root)
    }

    /// Every complete download of an archive's first volume can be
    /// unpacked, whether or not it was yet; nothing else shows unpacking.
    fn offer_extraction(&mut self) {
        if self.state.stage() != JobStage::Complete || !self.archive().is_some_and(|a| a.first) {
            self.extraction = None;
        } else if self.extraction.is_none() {
            self.extraction = Some(ExtractionSummary {
                state: ExtractionState::Idle,
                generation: None,
                output: None,
                problem: None,
            });
        }
    }

    /// The file name the job has or will have, if known.
    pub(super) fn name(&self) -> Option<&str> {
        match &self.artifact {
            Some(artifact) => artifact.path.rsplit('/').next(),
            None => self.name.as_deref(),
        }
    }

    pub(super) fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id,
            package: self.package,
            name: self.name.as_deref().map(Into::into),
            stage: self.state.stage(),
            desired: self.state.desired(),
            display: self.state.display(),
            revision: self.state.revision(),
            generation: self.state.generation(),
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            failure: self.failure.clone(),
            artifact: self.artifact.clone(),
            original_host: Some(self.original_host.clone()),
            destination: self.destination,
            restart_reason: self.restart_reason.clone(),
            route: self.route.clone().map(Box::new),
            media: self.media.clone().map(Box::new),
            extraction: self.extraction.clone().map(Box::new),
            check: self.check.clone().map(Box::new),
        }
    }
}

/// Row values before validation.
struct RawJob {
    id: i64,
    package: i64,
    desired: String,
    stage: String,
    generation: i64,
    revision: i64,
    bytes_total: Option<i64>,
    failure_code: Option<String>,
    failure_retry_at: Option<i64>,
    failure_message: Option<String>,
    bytes_done: i64,
    artifact: Option<(i64, String, i64)>,
    input_url: String,
    root: Option<i64>,
    removed: bool,
    restart_reason: Option<String>,
    route: (Option<String>, Option<i64>),
    name: Option<String>,
    media: (Option<String>, Option<String>),
    extraction: (Option<String>, Option<String>, Option<String>, Option<i64>),
    check: (Option<String>, bool),
}

impl RawJob {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            package: row.get(1)?,
            desired: row.get(2)?,
            stage: row.get(3)?,
            generation: row.get(4)?,
            revision: row.get(5)?,
            bytes_total: row.get(6)?,
            failure_code: row.get(7)?,
            failure_retry_at: row.get(8)?,
            failure_message: row.get(9)?,
            bytes_done: row.get(10)?,
            artifact: match row.get::<_, Option<i64>>(11)? {
                Some(root) => Some((root, row.get(12)?, row.get(13)?)),
                None => None,
            },
            input_url: row.get(14)?,
            root: row.get(15)?,
            removed: row.get(16)?,
            restart_reason: row.get(17)?,
            route: (row.get(18)?, row.get(19)?),
            name: row.get(20)?,
            media: (row.get(21)?, row.get(22)?),
            extraction: (row.get(23)?, row.get(24)?, row.get(25)?, row.get(26)?),
            check: (row.get(27)?, row.get(28)?),
        })
    }

    fn decode(self) -> Result<StoredJob, StoreError> {
        let desired = DesiredState::from_str(&self.desired).or_corrupt("desired state")?;
        let stage = JobStage::from_str(&self.stage).or_corrupt("stage")?;
        let state = JobState::restore(desired, stage, key(self.generation)?, key(self.revision)?)
            .or_corrupt("job state")?;
        let bytes = |v: i64| u64::try_from(v).or_corrupt("byte count");
        // Stored links were validated on submission, so each has a host.
        let original_host = url::Url::parse(&self.input_url)
            .ok()
            .and_then(|url| HostName::new(url.host_str()?).ok())
            .or_corrupt("source link")?;
        let name = self
            .name
            .as_deref()
            .and_then(crate::download::file_name)
            .or_else(|| source_name(&self.input_url))
            .map(|name| name.as_str().to_owned());
        let extraction = super::extractions::decode(self.extraction)?;
        let check = super::checks::decode(self.check.0, self.check.1)?;
        let mut job = StoredJob {
            id: key(self.id)?,
            state,
            package: key(self.package)?,
            bytes_total: self.bytes_total.map(bytes).transpose()?,
            bytes_done: bytes(self.bytes_done)?,
            failure: self
                .failure_code
                .map(|code| {
                    Ok::<_, StoreError>(FailureSummary {
                        code: FailureCode::from_str(&code).or_corrupt("failure code")?,
                        retry_at: self.failure_retry_at.map(UtcTimestamp::from_unix_millis),
                        message: self.failure_message.as_deref().map(DiagnosticText::new),
                    })
                })
                .transpose()?,
            artifact: self
                .artifact
                .map(|(root, path, size)| {
                    Ok::<_, StoreError>(ArtifactSummary {
                        root: key(root)?,
                        path,
                        size: bytes(size)?,
                    })
                })
                .transpose()?,
            original_host,
            destination: self.root.map(key).transpose()?,
            removed: self.removed,
            restart_reason: self.restart_reason.as_deref().map(DiagnosticText::new),
            route: operations::decode_route(self.route.0, self.route.1)?,
            name,
            media: decode_media(self.media.0, self.media.1.as_deref())?,
            extraction,
            check,
        };
        job.offer_extraction();
        Ok(job)
    }
}

/// A job's stored media service and choice.
fn decode_media(
    service: Option<String>,
    choice: Option<&str>,
) -> Result<Option<JobMedia>, StoreError> {
    service
        .map(|service| {
            Ok(JobMedia {
                service: ProviderId::new(service).or_corrupt("media service")?,
                choice: media::decode_choice(choice)?,
            })
        })
        .transpose()
}

/// Only filename-like source segments are useful public hints. Opaque path
/// IDs without extensions are omitted. The query, fragment, and user
/// information never enter the hint.
fn source_name(source: &str) -> Option<crate::fs::RelativePath> {
    let url = url::Url::parse(source).ok()?;
    let name = crate::download::url_file_name(&url)?;
    let (stem, extension) = name.as_str().rsplit_once('.')?;
    (!stem.is_empty()
        && !extension.is_empty()
        && extension.len() <= 16
        && extension.bytes().all(|b| b.is_ascii_alphanumeric()))
    .then_some(name)
}

/// Loads a job, or `NotFound`.
pub(super) fn load(tx: &Transaction<'_>, id: JobId) -> Result<StoredJob, StoreError> {
    // An ID beyond the stored range cannot exist.
    let Ok(id) = sql(id) else {
        return Err(StoreError::NotFound("job"));
    };
    tx.query_row(
        &format!("SELECT {JOB_COLUMNS} FROM {JOB_FROM} WHERE j.id = ?1"),
        [id],
        RawJob::read,
    )
    .optional()?
    .ok_or(StoreError::NotFound("job"))?
    .decode()
}

/// Loads a job the user has not removed, or `NotFound`: what clients may
/// see and change.
pub(super) fn load_listed(tx: &Transaction<'_>, id: JobId) -> Result<StoredJob, StoreError> {
    let job = load(tx, id)?;
    if job.removed {
        return Err(StoreError::NotFound("job"));
    }
    Ok(job)
}

/// Persists `job.state` over the row last read at `previous` revision, sets
/// the failure if the new stage is parked (clearing it otherwise), and
/// appends `job_changed`.
pub(super) fn save(
    tx: &Transaction<'_>,
    job: &mut StoredJob,
    previous: &JobState,
    failure: Option<FailureSummary>,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    let failure = failure.filter(|_| parked(job.state.stage()));
    let desired: &'static str = job.state.desired().into();
    let updated = tx.execute(
        "UPDATE jobs SET desired_state = ?1, stage = ?2, generation = ?3, revision = ?4, \
         failure_code = ?5, failure_retry_at = ?6, failure_message = ?7, updated_at = ?8 \
         WHERE id = ?9 AND revision = ?10",
        params![
            desired,
            job.state.stage().as_str(),
            sql(job.state.generation())?,
            sql(job.state.revision())?,
            failure.as_ref().map(|f| f.code.as_str()),
            failure
                .as_ref()
                .and_then(|f| f.retry_at)
                .map(UtcTimestamp::unix_millis),
            failure
                .as_ref()
                .and_then(|f| f.message.as_ref())
                .map(DiagnosticText::as_str),
            now.unix_millis(),
            sql(job.id)?,
            sql(previous.revision())?,
        ],
    )?;
    if updated != 1 {
        // The coordinator is the only writer, so this is a storage fault.
        return Err(StoreError::Corrupt("job revision"));
    }
    job.failure = failure;
    // Queued again under a new generation, the job starts from zero: no
    // generation will reopen its earlier files, except one a retry
    // reserved for exactly this generation.
    if job.state.stage() == JobStage::Queued && job.state.generation() != previous.generation() {
        files::abandon_unreserved(tx, job.id, job.state.generation())?;
    }
    // A finished or cancelled job needs no generated link any more.
    if matches!(job.state.stage(), JobStage::Complete | JobStage::Cancelled) {
        operations::retire_links(tx, job.id)?;
    }
    job.offer_extraction();
    outbox::append(
        tx,
        &EventKind::JobChanged {
            job: Box::new(job.summary()),
        },
        now,
    )?;
    Ok(())
}

/// Records why `job`'s latest attempt started from zero, or clears it.
pub(super) fn set_restart_reason(
    tx: &Transaction<'_>,
    job: &mut StoredJob,
    reason: Option<DiagnosticText>,
) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE jobs SET restart_reason = ?1 WHERE id = ?2",
        params![reason.as_ref().map(DiagnosticText::as_str), sql(job.id)?],
    )?;
    job.restart_reason = reason;
    Ok(())
}

/// Creates the queued job of source link `source`, saved in `root` and
/// downloaded through `route` (directly if `None`), and marks the link
/// staged, with a `job_added` event. The link's unique job makes a second
/// call for it fail rather than add another job.
pub(super) fn create(
    tx: &Transaction<'_>,
    source: i64,
    package: i64,
    root: Option<i64>,
    route: Option<&JobRoute>,
    media: Option<&NewMedia>,
    now: UtcTimestamp,
) -> Result<JobId, StoreError> {
    let state = JobState::new();
    let desired: &'static str = state.desired().into();
    let name = media
        .and_then(|m| crate::media::title_file_name(m.title.as_deref()?, None))
        .map(|name| name.as_str().to_owned());
    tx.execute(
        "INSERT INTO jobs (source_link_id, package_id, desired_state, stage, generation, \
         revision, root_id, route_service, route_account, created_at, updated_at, \
         media_service, media_choice, suggested_name) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, ?12, ?13)",
        params![
            source,
            package,
            desired,
            state.stage().as_str(),
            sql(state.generation())?,
            sql(state.revision())?,
            root,
            route.map(|r| r.service.as_str()),
            route.map(|r| sql(r.account)).transpose()?,
            now.unix_millis(),
            media.map(|m| m.service.as_str()),
            media.map(|m| media::encode_choice(&m.choice)),
            name,
        ],
    )?;
    let id: JobId = key(tx.last_insert_rowid())?;
    tx.execute(
        "UPDATE source_links SET staged = 1, held = 0, problem = NULL WHERE id = ?1",
        [source],
    )?;
    // A started link's check would tell nothing the download will not.
    super::crawl::drop_check(tx, source)?;
    let job = load(tx, id)?;
    outbox::append(
        tx,
        &EventKind::JobAdded {
            job: Box::new(job.summary()),
        },
        now,
    )?;
    Ok(id)
}

/// Appends `job_changed` for a change outside the job's state, such as its
/// expected size.
pub(super) fn announce(
    tx: &Transaction<'_>,
    id: JobId,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    let job = load(tx, id)?;
    outbox::append(
        tx,
        &EventKind::JobChanged {
            job: Box::new(job.summary()),
        },
        now,
    )?;
    Ok(())
}

impl Store {
    /// Records an accepted provider's filename before requesting its bytes.
    /// Late work cannot rename a paused, cancelled, removed, or newer job.
    pub(crate) fn record_suggested_name(
        &mut self,
        id: JobId,
        generation: wayfetch_domain::version::Generation,
        name: &str,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let Some(name) = crate::download::file_name(name) else {
            return Ok(());
        };
        let tx = self.write()?;
        let job = load(&tx, id)?;
        if job.state.generation() == generation
            && job.state.stage().is_active()
            && job.state.stage() != JobStage::Publishing
            && job.state.desired() == DesiredState::Running
            && !job.removed
        {
            let updated = tx.execute(
                "UPDATE jobs SET suggested_name = ?1, updated_at = ?2 WHERE id = ?3 \
                 AND suggested_name IS NOT ?1",
                params![name.as_str(), now.unix_millis(), sql(id)?],
            )?;
            if updated != 0 {
                announce(&tx, id, now)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The ingestion step for direct links: creates a queued job for up to
    /// `limit` submitted links that have none, oldest first, each with a
    /// `job_added` event. Collected links wait to be started instead. Safe
    /// to repeat after a crash; returns the new jobs.
    pub fn stage_pending(
        &mut self,
        limit: u32,
        now: UtcTimestamp,
    ) -> Result<Vec<JobId>, StoreError> {
        let tx = self.write()?;
        let pending: Vec<(i64, i64, Option<i64>)> = tx
            .prepare(
                "SELECT s.id, b.package_id, b.root_id FROM source_links s \
                 JOIN input_batches b ON b.id = s.batch_id \
                 WHERE s.staged = 0 AND s.held = 0 ORDER BY s.id LIMIT ?1",
            )?
            .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let staged = pending
            .into_iter()
            .map(|(source, package, root)| create(&tx, source, package, root, None, None, now))
            .collect::<Result<Vec<_>, _>>()?;
        tx.commit()?;
        Ok(staged)
    }

    /// Moves a job to `to` on behalf of work that holds generation
    /// `presented`. A stale generation is rejected, so a late worker from a
    /// cancelled, reset, or pre-restart run cannot change the job. Parked
    /// stages (waiting, blocked, input needed, failed) need the failure that
    /// explains them.
    pub fn advance_job(
        &mut self,
        id: JobId,
        to: JobStage,
        presented: Generation,
        failure: Option<Failure>,
        now: UtcTimestamp,
    ) -> Result<Outcome, StoreError> {
        let failure = failure.map(|f| FailureSummary {
            code: f.code(),
            retry_at: f.earliest_retry(),
            message: f.context().cloned(),
        });
        self.record_stage(id, to, presented, failure, now)
    }

    /// [`Store::advance_job`] with a failure whose scope has no durable
    /// record yet, such as a transfer attempt. Only the summary is stored.
    pub(crate) fn record_stage(
        &mut self,
        id: JobId,
        to: JobStage,
        presented: Generation,
        failure: Option<FailureSummary>,
        now: UtcTimestamp,
    ) -> Result<Outcome, StoreError> {
        if parked(to) && failure.is_none() {
            return Err(StoreError::MissingFailure(to));
        }
        let tx = self.write()?;
        let mut job = load(&tx, id)?;
        let previous = job.state;
        let outcome = job.state.advance(to, presented)?;
        if outcome == Outcome::Changed {
            save(&tx, &mut job, &previous, failure, now)?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// Returns an active job of generation `presented` to the queue under a
    /// new generation, which starts from zero, recording `reason` so a
    /// client can say why its saved data was not continued. A transferring
    /// job steps back to resolving first. One transaction, so the reason is
    /// never shown without the restart, or the restart without it.
    pub(crate) fn restart_from_zero(
        &mut self,
        id: JobId,
        presented: Generation,
        reason: &str,
        now: UtcTimestamp,
    ) -> Result<Outcome, StoreError> {
        let tx = self.write()?;
        let mut job = load(&tx, id)?;
        if job.state.stage() == JobStage::Transferring {
            let previous = job.state;
            job.state.advance(JobStage::Resolving, presented)?;
            save(&tx, &mut job, &previous, None, now)?;
        }
        let previous = job.state;
        let outcome = job.state.advance(JobStage::Queued, presented)?;
        if outcome == Outcome::Changed {
            set_restart_reason(&tx, &mut job, Some(DiagnosticText::new(reason)))?;
            save(&tx, &mut job, &previous, None, now)?;
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// Up to `limit` jobs ready for the coordinator to pick up, skipping
    /// `running`: recovering jobs first, then queued ones, oldest first.
    /// Jobs without a destination are included only if `unassigned`, when
    /// there is a default to give them. The scheduler reads only as many as
    /// it has free slots, so waiting jobs stay in the database rather than
    /// in memory.
    pub fn admissible(
        &mut self,
        limit: usize,
        running: &[JobId],
        unassigned: bool,
    ) -> Result<Vec<Admission>, StoreError> {
        let tx = self.read()?;
        let fetch = i64::try_from(limit.saturating_add(running.len())).unwrap_or(i64::MAX);
        type Row = (
            i64,
            String,
            i64,
            String,
            String,
            Option<i64>,
            Option<String>,
            Option<i64>,
            (Option<String>, Option<String>),
            bool,
        );
        let rows: Vec<Row> = tx
            .prepare(
                "SELECT j.id, j.stage, j.generation, j.desired_state, s.input_url, j.root_id, \
                 j.route_service, j.route_account, j.media_service, j.media_choice, \
                 s.public_only FROM jobs j JOIN source_links s ON s.id = j.source_link_id \
                 WHERE j.stage IN ('queued', 'recovering') AND (?2 OR j.root_id IS NOT NULL) \
                 ORDER BY j.stage = 'recovering' DESC, j.id LIMIT ?1",
            )?
            .query_map(params![fetch, unassigned], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    (row.get(8)?, row.get(9)?),
                    row.get(10)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let mut ready = Vec::with_capacity(limit);
        for (id, stage, generation, desired, url, root, service, account, media, public_only) in
            rows
        {
            let id: JobId = key(id)?;
            if running.contains(&id) || ready.len() == limit {
                continue;
            }
            ready.push(Admission {
                job: id,
                stage: JobStage::from_str(&stage).or_corrupt("stage")?,
                generation: key(generation)?,
                desired: DesiredState::from_str(&desired).or_corrupt("desired state")?,
                url,
                root: root.map(key).transpose()?,
                route: operations::decode_route(service, account)?,
                media: decode_media(media.0, media.1.as_deref())?,
                reach: super::crawl::reach(public_only),
            });
        }
        Ok(ready)
    }

    /// Gives a job submitted without a destination the root it will be
    /// saved in, with a `job_changed` event. A job that has one keeps it.
    pub fn assign_root(
        &mut self,
        id: JobId,
        root: RootId,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        let updated = tx.execute(
            "UPDATE jobs SET root_id = ?1, updated_at = ?2 WHERE id = ?3 AND root_id IS NULL",
            params![sql(root)?, now.unix_millis(), sql(id)?],
        )?;
        if updated == 1 {
            announce(&tx, id, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The job's stage, desired state, and generation.
    pub fn job_state(&mut self, id: JobId) -> Result<JobState, StoreError> {
        let tx = self.read()?;
        Ok(load(&tx, id)?.state)
    }

    /// Runs once at startup, before any work. Every job left in an active
    /// stage by the previous process moves to recovery under a new
    /// generation, which retires anything that process authorized. Returns
    /// the number of jobs moved.
    pub fn recover_after_restart(&mut self, now: UtcTimestamp) -> Result<usize, StoreError> {
        let active: Vec<&'static str> = JobStage::VARIANTS
            .iter()
            .filter(|s| s.is_active())
            .map(|s| s.as_str())
            .collect();
        let tx = self.write()?;
        let ids: Vec<i64> = tx
            .prepare(&format!(
                "SELECT id FROM jobs WHERE stage IN ({}) ORDER BY id",
                placeholders(active.len())
            ))?
            .query_map(params_from_iter(&active), |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        for id in &ids {
            let mut job = load(&tx, key(*id)?)?;
            let previous = job.state;
            if job.state.recover_after_restart()? == Outcome::Changed {
                save(&tx, &mut job, &previous, None, now)?;
            }
        }
        tx.commit()?;
        Ok(ids.len())
    }

    /// Answers a read query from one consistent snapshot.
    pub fn query(
        &mut self,
        query: &Query,
        instance: InstanceId,
    ) -> Result<QueryResult, ProtocolError> {
        let converter = self.media_converter;
        let tx = self.read().map_err(ProtocolError::from)?;
        let as_of = EventCursor {
            instance,
            position: outbox::head(&tx)?,
        };
        let result = match query {
            Query::InspectJob { job } => QueryResult::Job {
                job: load_listed(&tx, *job)?.summary(),
                as_of,
            },
            Query::JobSource { job } => {
                load_listed(&tx, *job)?;
                let url = tx
                    .query_row(
                        "SELECT s.input_url FROM jobs j JOIN source_links s \
                         ON s.id = j.source_link_id WHERE j.id = ?1",
                        [sql(*job)?],
                        |row| row.get(0),
                    )
                    .map_err(StoreError::from)?;
                QueryResult::JobSource { job: *job, url }
            }
            Query::ListJobs {
                filter,
                after,
                page_size,
            } => {
                let (conditions, mut values) = filter_clause(filter);
                let after = match after {
                    // Past the stored range: nothing follows.
                    Some(id) => sql(*id).unwrap_or(i64::MAX),
                    None => 0,
                };
                values.insert(0, after.into());
                let before: i64 = tx
                    .query_row(
                        &format!("SELECT COUNT(*) FROM jobs j WHERE j.id <= ?{conditions}"),
                        params_from_iter(values.iter()),
                        |row| row.get(0),
                    )
                    .map_err(StoreError::from)?;
                // One extra row tells whether another page follows.
                values.push((i64::from(page_size.get()) + 1).into());
                let mut items = tx
                    .prepare(&format!(
                        "SELECT {JOB_COLUMNS} FROM {JOB_FROM} WHERE j.id > ?{conditions} \
                         ORDER BY j.id LIMIT ?"
                    ))
                    .map_err(StoreError::from)?
                    .query_map(params_from_iter(values), RawJob::read)
                    .map_err(StoreError::from)?
                    .map(|raw| Ok(raw?.decode()?.summary()))
                    .collect::<Result<Vec<_>, StoreError>>()?;
                let next_after = if items.len() > usize::from(page_size.get()) {
                    items.truncate(usize::from(page_size.get()));
                    items.last().map(|j| j.id)
                } else {
                    None
                };
                QueryResult::Jobs(Page {
                    items,
                    next_after,
                    before: Some(u64::try_from(before).or_corrupt("job count")?),
                    as_of,
                })
            }
            Query::CountJobs { filter } => {
                let (conditions, values) = filter_clause(filter);
                let stages = tx
                    .prepare(&format!(
                        "SELECT j.stage, COUNT(*) FROM jobs j WHERE 1{conditions} GROUP BY j.stage"
                    ))
                    .map_err(StoreError::from)?
                    .query_map(params_from_iter(values), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })
                    .map_err(StoreError::from)?
                    .map(|row| {
                        let (stage, count) = row?;
                        Ok((
                            JobStage::from_str(&stage).or_corrupt("stage")?,
                            u64::try_from(count).or_corrupt("count")?,
                        ))
                    })
                    .collect::<Result<_, StoreError>>()?;
                QueryResult::JobCounts(JobCounts { stages, as_of })
            }
            Query::ListLinks { after, page_size } => {
                QueryResult::Links(links::list(&tx, *after, *page_size, as_of, converter)?)
            }
            Query::LinkSource { link } => QueryResult::LinkSource {
                link: *link,
                url: links::source(&tx, *link)?,
            },
            // Availability is checked outside the store, by the coordinator.
            Query::Settings {}
            | Query::Accounts {}
            | Query::Authorization { .. }
            | Query::Updates { .. } => {
                return Err(ProtocolError::new(
                    wayfetch_protocol::ErrorCode::Internal,
                    "settings, accounts, and updates are answered by the coordinator",
                ));
            }
        };
        Ok(result)
    }
}

/// `?, ?, ...` with `n` entries.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// ` AND ...` conditions on `jobs j` selecting the listed jobs `filter`
/// names, and their values in order.
fn filter_clause(filter: &JobFilter) -> (String, Vec<Value>) {
    let mut conditions = String::from(" AND j.removed_at IS NULL");
    let mut values = Vec::new();
    if let Some(package) = filter.package {
        conditions.push_str(" AND j.package_id = ?");
        // A package beyond the stored range matches nothing.
        values.push(sql(package).unwrap_or(-1).into());
    }
    if !filter.stages.is_empty() {
        conditions.push_str(&format!(
            " AND j.stage IN ({})",
            placeholders(filter.stages.len())
        ));
        values.extend(filter.stages.iter().map(|s| s.as_str().to_owned().into()));
    }
    (conditions, values)
}
