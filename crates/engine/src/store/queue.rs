//! The queue's order: waiting jobs start in ascending `queue_rank`, then
//! ID, read a bounded window at a time (architecture §7.3). Moving jobs to
//! the top or bottom gives them ranks below or above every job's, so only
//! the moved rows change; lists in queue order page by the same key.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::JobId;
use wayfetch_domain::job::JobError;
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::command::{JobRefs, QueueEnd, RefusedJob};
use wayfetch_protocol::query::JobOrder;
use wayfetch_protocol::{CommandResult, ErrorCode, ProtocolError};

use super::{OrCorrupt, StoreError, jobs, sql};

const GONE: &str = "This download is no longer in the list.";
const CHANGED: &str = "This download changed since the list was shown. Check it and try again.";
const NOT_WAITING: &str = "Only downloads waiting to start can be moved in the queue.";

/// Moves the waiting jobs of `refs` to the `to` end of the queue, keeping
/// the order they were queued in, each with a `job_changed` event. A job
/// that is gone, changed, or not waiting is refused and left as it is.
pub(super) fn move_jobs(
    tx: &Transaction<'_>,
    refs: &JobRefs,
    to: QueueEnd,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let mut waiting = Vec::new();
    let mut refused = Vec::new();
    for named in refs.as_slice() {
        let checked = match jobs::load_listed(tx, named.job) {
            Ok(job) => job.state.check_waiting(named.revision),
            Err(StoreError::NotFound(_)) => {
                refused.push(refusal(named.job, ErrorCode::NotFound, GONE));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        match checked {
            Ok(()) => {
                let rank = rank(tx, sql(named.job)?)?.ok_or(StoreError::NotFound("job"))?;
                waiting.push((rank, named.job));
            }
            Err(JobError::RevisionConflict { .. }) => {
                refused.push(refusal(named.job, ErrorCode::RevisionConflict, CHANGED));
            }
            Err(_) => refused.push(refusal(named.job, ErrorCode::NotApplicable, NOT_WAITING)),
        }
    }
    waiting.sort_unstable();
    let count = i64::try_from(waiting.len()).or_corrupt("job count")?;
    if count > 0 {
        // Below or above every job's rank, not only the waiting ones', so
        // a job moved earlier and paused meanwhile does not overtake these
        // when it waits again.
        let first = match to {
            QueueEnd::Top => bound(tx, "MIN")?.checked_sub(count),
            QueueEnd::Bottom => bound(tx, "MAX")?
                .checked_add(count)
                .map(|last| last - count + 1),
        }
        .ok_or(StoreError::Corrupt("queue rank"))?;
        let mut place = tx
            .prepare("UPDATE jobs SET queue_rank = ?1, updated_at = ?2 WHERE id = ?3")
            .map_err(StoreError::from)?;
        for (rank, (_, job)) in (first..).zip(&waiting) {
            place
                .execute(params![rank, now.unix_millis(), sql(*job)?])
                .map_err(StoreError::from)?;
            jobs::announce(tx, *job, now)?;
        }
    }
    Ok(CommandResult::JobsMoved {
        moved: waiting.into_iter().map(|(_, job)| job).collect(),
        refused,
    })
}

fn refusal(job: JobId, code: ErrorCode, message: &str) -> RefusedJob {
    RefusedJob {
        job,
        code,
        message: DiagnosticText::new(message),
    }
}

/// The place in the queue of the job with ID `id`, if there is one.
fn rank(tx: &Transaction<'_>, id: i64) -> Result<Option<i64>, StoreError> {
    Ok(tx
        .query_row("SELECT queue_rank FROM jobs WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?)
}

/// The lowest (`MIN`) or highest (`MAX`) rank of any job. One aggregate
/// per query, so SQLite reads it from the end of `jobs_by_queue_rank`.
fn bound(tx: &Transaction<'_>, aggregate: &str) -> Result<i64, StoreError> {
    Ok(tx.query_row(
        &format!("SELECT COALESCE({aggregate}(queue_rank), 0) FROM jobs"),
        [],
        |row| row.get(0),
    )?)
}

/// How a page of jobs listed in some order is read: conditions on `jobs j`
/// selecting the jobs after the cursor and up to it, the ORDER BY, and
/// the cursor's values, which either condition takes first.
pub(super) struct Keyset {
    pub(super) after: &'static str,
    pub(super) through: &'static str,
    pub(super) order: &'static str,
    pub(super) values: Vec<Value>,
}

/// The keyset of a list in `order` continuing after job `after`. In queue
/// order the cursor is that job's place now.
pub(super) fn keyset(
    tx: &Transaction<'_>,
    order: JobOrder,
    after: Option<JobId>,
) -> Result<Keyset, StoreError> {
    // A job beyond the stored range, or unknown, has nothing after it.
    let id = after.map(|job| sql(job).unwrap_or(i64::MAX));
    Ok(match order {
        JobOrder::Added => Keyset {
            after: "j.id > ?",
            through: "j.id <= ?",
            order: "j.id",
            values: vec![id.unwrap_or(0).into()],
        },
        JobOrder::Queue => {
            let cursor = match id {
                None => (i64::MIN, 0),
                Some(id) => rank(tx, id)?.map_or((i64::MAX, i64::MAX), |rank| (rank, id)),
            };
            Keyset {
                after: "(j.queue_rank, j.id) > (?, ?)",
                through: "(j.queue_rank, j.id) <= (?, ?)",
                order: "j.queue_rank, j.id",
                values: vec![cursor.0.into(), cursor.1.into()],
            }
        }
    })
}
