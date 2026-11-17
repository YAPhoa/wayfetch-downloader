//! Automatic retries of transient failures (architecture section 7.4,
//! ADR-13): a job whose attempt failed transiently waits in the queue until
//! a durable deadline, its partial reserved for the next generation as a
//! manual retry reserves it, until its retries in a row without new durable
//! data run out.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::failure::RetryBudget;
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::{JobStage, Outcome};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::query::{FailureSummary, RetryAttempt};

use super::{OrCorrupt, Store, StoreError, files, jobs, operations, sql};
use crate::config::Retries;

/// A job's stored retry state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RetryRow {
    /// Retries made in a row without new durable data.
    pub(super) attempt: u32,
    /// While waiting: the most retries in a row allowed.
    pub(super) limit: Option<u32>,
    /// While waiting: the end of the wait the server asked for.
    pub(super) not_before: Option<UtcTimestamp>,
}

impl RetryRow {
    pub(super) fn decode(
        (attempt, limit, not_before): (i64, Option<i64>, Option<i64>),
    ) -> Result<Self, StoreError> {
        Ok(Self {
            attempt: u32::try_from(attempt).or_corrupt("retry attempt")?,
            limit: limit
                .map(u32::try_from)
                .transpose()
                .or_corrupt("retry limit")?,
            not_before: not_before.map(UtcTimestamp::from_unix_millis),
        })
    }

    /// The retry a waiting job waits for.
    pub(super) fn attempt(&self) -> Option<RetryAttempt> {
        Some(RetryAttempt {
            number: self.attempt,
            limit: self.limit?,
        })
    }
}

/// Stores `row` as `job`'s retry state.
fn store(tx: &Transaction<'_>, job: &mut jobs::StoredJob, row: RetryRow) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE jobs SET retry_attempt = ?1, retry_limit = ?2, retry_not_before = ?3 \
         WHERE id = ?4",
        params![
            row.attempt,
            row.limit,
            row.not_before.map(UtcTimestamp::unix_millis),
            sql(job.id)?
        ],
    )?;
    job.retry = row;
    Ok(())
}

/// Clears `job`'s retry state: a person retried it, or it gave up.
pub(super) fn reset(tx: &Transaction<'_>, job: &mut jobs::StoredJob) -> Result<(), StoreError> {
    store(tx, job, RetryRow::default())
}

/// Deadlines at or before the first value are due at `now`; so are those
/// after the second, which `retries` never schedules (a clock set back).
pub(super) fn due_window(retries: &Retries, now: UtcTimestamp) -> (i64, i64) {
    let latest = now.checked_add(retries.horizon()).unwrap_or(now);
    (now.unix_millis(), latest.unix_millis())
}

/// Whether deadline `at` still lies ahead at `now`.
pub(super) fn ahead(retries: &Retries, at: UtcTimestamp, now: UtcTimestamp) -> bool {
    let (due, implausible) = due_window(retries, now);
    (due + 1..=implausible).contains(&at.unix_millis())
}

/// Durable bytes of `job`'s partial at `generation`.
fn durable(tx: &Transaction<'_>, job: JobId, generation: Generation) -> Result<u64, StoreError> {
    let bytes: Option<i64> = tx
        .query_row(
            "SELECT durable_bytes FROM partials WHERE job_id = ?1 AND generation = ?2",
            params![sql(job)?, sql(generation)?],
            |row| row.get(0),
        )
        .optional()?;
    u64::try_from(bytes.unwrap_or(0)).or_corrupt("durable bytes")
}

impl Store {
    /// How transient failures are retried. The coordinator sets it once at
    /// start.
    pub(crate) fn set_retries(&mut self, retries: crate::config::Retries) {
        self.retries = retries;
    }

    /// Settles an attempt of generation `presented` that ended with
    /// `failure`, whose code asks for a retry with backoff. Unless the job
    /// is publishing, its provider operation's outcome is unknown, or its
    /// retries in a row without new data are spent, it is queued again under
    /// a new generation that continues its partial, waiting for a deadline:
    /// the backoff for this retry, picked with `random`, but no sooner than
    /// the server's `failure.retry_at`. Otherwise it fails, saying how many
    /// attempts it made. `resumed_from` is the partial's total durable bytes
    /// when the attempt started, if known: an attempt that added to them,
    /// wherever in the file, starts the count again.
    pub(crate) fn wait_to_retry(
        &mut self,
        id: JobId,
        presented: Generation,
        failure: FailureSummary,
        resumed_from: Option<u64>,
        random: u32,
        now: UtcTimestamp,
    ) -> Result<Outcome, StoreError> {
        let retries = self.retries;
        let tx = self.write()?;
        let mut job = jobs::load(&tx, id)?;
        let progressed = match resumed_from {
            Some(from) => durable(&tx, id, presented)? > from,
            None => false,
        };
        let used = if progressed { 0 } else { job.retry.attempt };
        let budget = RetryBudget::restore(used, retries.attempts).consume();
        let retryable =
            job.state.stage() != JobStage::Publishing && !operations::outcome_unknown(&tx, id)?;
        let outcome = match budget {
            Some(budget) if retryable => {
                let number = budget.used();
                let not_before = failure
                    .retry_at
                    .map(|at| at.min(now.checked_add(retries.max_server_delay).unwrap_or(at)));
                let backoff = now
                    .checked_add(retries.delay(number, random))
                    .or_corrupt("retry deadline")?;
                let deadline = not_before.map_or(backoff, |at| at.max(backoff));
                jobs::step_back(&tx, &mut job, presented, now)?;
                let previous = job.state;
                let outcome = job.state.advance(JobStage::Queued, presented)?;
                // Before the save, which abandons every file not reserved
                // for the new generation.
                files::reserve_for_retry(&tx, id, job.state.generation())?;
                let row = RetryRow {
                    attempt: number,
                    limit: Some(retries.attempts),
                    not_before,
                };
                store(&tx, &mut job, row)?;
                let waiting = FailureSummary {
                    retry_at: Some(deadline),
                    attempt: row.attempt(),
                    ..failure
                };
                jobs::save(&tx, &mut job, &previous, Some(waiting), now)?;
                outcome
            }
            _ => {
                let previous = job.state;
                let outcome = job.state.advance(JobStage::Failed, presented)?;
                let attempts = used.saturating_add(1);
                let message = match (&failure.message, budget) {
                    (Some(message), None) if attempts > 1 => Some(DiagnosticText::new(&format!(
                        "{}; gave up after {attempts} attempts in a row without new data",
                        message.as_str()
                    ))),
                    (message, _) => message.clone(),
                };
                reset(&tx, &mut job)?;
                let failed = FailureSummary {
                    retry_at: None,
                    message,
                    ..failure
                };
                jobs::save(&tx, &mut job, &previous, Some(failed), now)?;
                outcome
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// The earliest automatic retry still ahead at `now`, for the
    /// coordinator's one timer. Deadlines already due are not included:
    /// those jobs are admissible now.
    pub(crate) fn next_retry(
        &mut self,
        now: UtcTimestamp,
    ) -> Result<Option<UtcTimestamp>, StoreError> {
        let (due, implausible) = due_window(&self.retries, now);
        let tx = self.read()?;
        let next: Option<i64> = tx.query_row(
            "SELECT MIN(failure_retry_at) FROM jobs WHERE stage = 'queued' \
             AND failure_retry_at > ?1 AND failure_retry_at <= ?2",
            params![due, implausible],
            |row| row.get(0),
        )?;
        Ok(next.map(UtcTimestamp::from_unix_millis))
    }
}
