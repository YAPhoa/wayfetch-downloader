//! Job rows: staging, generation-checked transitions, restart recovery, and
//! read queries.

use std::str::FromStr;

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};
use strum::VariantArray;
use wayfetch_domain::failure::{Failure, FailureCode};
use wayfetch_domain::id::{JobId, PackageId};
use wayfetch_domain::job::{DesiredState, JobStage, JobState, Outcome};
use wayfetch_domain::name::{DiagnosticText, HostName};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::event::{EventCursor, EventKind};
use wayfetch_protocol::query::{
    ArtifactSummary, FailureSummary, JobCounts, JobFilter, JobSummary, Page, Query, QueryResult,
};
use wayfetch_protocol::{InstanceId, ProtocolError};

use super::{Store, StoreError, files, key, outbox, sql};

/// Columns read by [`RawJob::read`]. `bytes_done` is the durable progress of
/// the job's newest partial; the next three describe its published file,
/// and the last is the submitted link.
const JOB_COLUMNS: &str = "j.id, j.package_id, j.desired_state, j.stage, j.generation, \
     j.revision, j.bytes_total, j.failure_code, j.failure_retry_at, j.failure_message, \
     COALESCE((SELECT p.durable_bytes FROM partials p WHERE p.job_id = j.id \
               ORDER BY p.generation DESC LIMIT 1), 0), \
     a.root_id, a.target_path, a.size_bytes, s.input_url";

/// The tables [`JOB_COLUMNS`] reads. A job publishes at most one file and
/// has exactly one source link.
const JOB_FROM: &str = "jobs j JOIN source_links s ON s.id = j.source_link_id \
     LEFT JOIN publications a ON a.job_id = j.id AND a.state = 'published'";

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
}

/// Stages that are parked with a failure explaining why.
fn parked(stage: JobStage) -> bool {
    matches!(
        stage,
        JobStage::Waiting | JobStage::Blocked | JobStage::NeedsInput | JobStage::Failed
    )
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
}

impl StoredJob {
    pub(super) fn summary(&self) -> JobSummary {
        JobSummary {
            id: self.id,
            package: self.package,
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
        })
    }

    fn decode(self) -> Result<StoredJob, StoreError> {
        let desired = DesiredState::from_str(&self.desired)
            .map_err(|_| StoreError::Corrupt("desired state"))?;
        let stage = JobStage::from_str(&self.stage).map_err(|_| StoreError::Corrupt("stage"))?;
        let state = JobState::restore(desired, stage, key(self.generation)?, key(self.revision)?)
            .map_err(|_| StoreError::Corrupt("job state"))?;
        let bytes = |v: i64| u64::try_from(v).map_err(|_| StoreError::Corrupt("byte count"));
        // Stored links were validated on submission, so each has a host.
        let original_host = url::Url::parse(&self.input_url)
            .ok()
            .and_then(|url| HostName::new(url.host_str()?).ok())
            .ok_or(StoreError::Corrupt("source link"))?;
        Ok(StoredJob {
            id: key(self.id)?,
            state,
            package: key(self.package)?,
            bytes_total: self.bytes_total.map(bytes).transpose()?,
            bytes_done: bytes(self.bytes_done)?,
            failure: self
                .failure_code
                .map(|code| {
                    Ok::<_, StoreError>(FailureSummary {
                        code: FailureCode::from_str(&code)
                            .map_err(|_| StoreError::Corrupt("failure code"))?,
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
        })
    }
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
    // generation will reopen its earlier files.
    if job.state.stage() == JobStage::Queued && job.state.generation() != previous.generation() {
        files::abandon_staging(tx, job.id, job.state.generation())?;
    }
    outbox::append(tx, &EventKind::JobChanged { job: job.summary() }, now)?;
    Ok(())
}

/// Appends `job_changed` for a change outside the job's state, such as its
/// expected size.
pub(super) fn announce(
    tx: &Transaction<'_>,
    id: JobId,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    let job = load(tx, id)?;
    outbox::append(tx, &EventKind::JobChanged { job: job.summary() }, now)?;
    Ok(())
}

impl Store {
    /// The ingestion step for direct links: creates a queued job for up to
    /// `limit` submitted links that have none, oldest first, each with a
    /// `job_added` event. Safe to repeat after a crash; returns the new jobs.
    pub fn stage_pending(
        &mut self,
        limit: u32,
        now: UtcTimestamp,
    ) -> Result<Vec<JobId>, StoreError> {
        let tx = self.write()?;
        let pending: Vec<(i64, i64)> = tx
            .prepare(
                "SELECT s.id, b.package_id FROM source_links s \
                 JOIN input_batches b ON b.id = s.batch_id \
                 WHERE s.staged = 0 ORDER BY s.id LIMIT ?1",
            )?
            .query_map([limit], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let state = JobState::new();
        let desired: &'static str = state.desired().into();
        let mut staged = Vec::with_capacity(pending.len());
        for (source, package) in pending {
            tx.execute(
                "INSERT INTO jobs (source_link_id, package_id, desired_state, stage, generation, \
                 revision, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![
                    source,
                    package,
                    desired,
                    state.stage().as_str(),
                    sql(state.generation())?,
                    sql(state.revision())?,
                    now.unix_millis(),
                ],
            )?;
            let id: JobId = key(tx.last_insert_rowid())?;
            tx.execute("UPDATE source_links SET staged = 1 WHERE id = ?1", [source])?;
            let job = load(&tx, id)?;
            outbox::append(&tx, &EventKind::JobAdded { job: job.summary() }, now)?;
            staged.push(id);
        }
        tx.commit()?;
        Ok(staged)
    }

    /// Moves a job to `to` on behalf of work that holds generation
    /// `presented`. A stale generation is rejected, so a late worker from a
    /// cancelled, reset, or pre-restart run cannot change the job (T17,
    /// T27). Parked stages (waiting, blocked, input needed, failed) need the
    /// failure that explains them.
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

    /// Up to `limit` jobs ready for the coordinator to pick up, skipping
    /// `running`: recovering jobs first, then queued ones, oldest first.
    /// The scheduler reads only as many as it has free slots, so waiting
    /// jobs stay in the database rather than in memory.
    pub fn admissible(
        &mut self,
        limit: usize,
        running: &[JobId],
    ) -> Result<Vec<Admission>, StoreError> {
        let tx = self.read()?;
        let fetch = i64::try_from(limit.saturating_add(running.len())).unwrap_or(i64::MAX);
        let rows: Vec<(i64, String, i64, String, String)> = tx
            .prepare(
                "SELECT j.id, j.stage, j.generation, j.desired_state, s.input_url \
                 FROM jobs j JOIN source_links s ON s.id = j.source_link_id \
                 WHERE j.stage IN ('queued', 'recovering') \
                 ORDER BY j.stage = 'recovering' DESC, j.id LIMIT ?1",
            )?
            .query_map([fetch], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let mut ready = Vec::with_capacity(limit);
        for (id, stage, generation, desired, url) in rows {
            let id: JobId = key(id)?;
            if running.contains(&id) || ready.len() == limit {
                continue;
            }
            ready.push(Admission {
                job: id,
                stage: JobStage::from_str(&stage).map_err(|_| StoreError::Corrupt("stage"))?,
                generation: key(generation)?,
                desired: DesiredState::from_str(&desired)
                    .map_err(|_| StoreError::Corrupt("desired state"))?,
                url,
            });
        }
        Ok(ready)
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
        let tx = self.read().map_err(ProtocolError::from)?;
        let as_of = EventCursor {
            instance,
            position: outbox::head(&tx)?,
        };
        let result = match query {
            Query::InspectJob { job } => QueryResult::Job {
                job: load(&tx, *job)?.summary(),
                as_of,
            },
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
                            JobStage::from_str(&stage).map_err(|_| StoreError::Corrupt("stage"))?,
                            u64::try_from(count).map_err(|_| StoreError::Corrupt("count"))?,
                        ))
                    })
                    .collect::<Result<_, StoreError>>()?;
                QueryResult::JobCounts(JobCounts { stages, as_of })
            }
        };
        Ok(result)
    }
}

/// `?, ?, ...` with `n` entries.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// ` AND ...` conditions on `jobs j` selecting the jobs `filter` names,
/// and their values in order.
fn filter_clause(filter: &JobFilter) -> (String, Vec<Value>) {
    let mut conditions = String::new();
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
