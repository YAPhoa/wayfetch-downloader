//! Provider operations: the durable record of asking a provider for a
//! download link (ADR-09).
//!
//! The intent commits before the request is sent and the outcome before any
//! transfer, so nothing is repeated blindly. A job has at most one open
//! operation, and generated links are stored sealed.

use std::str::FromStr;

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{AccountId, JobId};
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::query::JobRoute;

use super::{Store, StoreError, jobs, key, sql};
use crate::accounts::vault::{REFERENCE_BYTES, Reference};

/// The open states, as the schema's index names them.
const OPEN: &str = "('intended', 'succeeded', 'ambiguous', 'confirmed')";

/// Where an operation stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum OperationState {
    /// Sent, or about to be; the outcome is not known.
    Intended,
    /// The provider generated a link, stored sealed.
    Succeeded,
    /// The provider certainly generated nothing.
    Failed,
    /// The answer was lost and no record shows the outcome.
    Ambiguous,
    /// The user confirmed repeating an ambiguous operation.
    Confirmed,
    /// No longer used.
    Retired,
}

/// A job's open operation.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Operation {
    pub(crate) id: i64,
    pub(crate) state: OperationState,
    pub(crate) service: ProviderId,
    pub(crate) account: AccountId,
    pub(crate) started_at: UtcTimestamp,
    /// The sealed link of a succeeded operation, and its reference.
    pub(crate) sealed: Option<(Reference, Vec<u8>)>,
    /// The account's sign-in a succeeded operation's link came with.
    pub(crate) sign_in: Option<Generation>,
}

impl std::fmt::Debug for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Operation")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// How an operation ended.
#[derive(Debug)]
pub(crate) enum OperationOutcome {
    /// A link, sealed for the operation's service, generated or found
    /// with the account's sign-in `sign_in`.
    Succeeded {
        reference: Reference,
        sealed: Vec<u8>,
        sign_in: Generation,
    },
    /// Nothing was generated.
    Failed(String),
    /// Unknown, and no record shows it.
    Ambiguous(String),
}

/// The job's open operation, if any.
pub(super) fn open(tx: &Transaction<'_>, job: JobId) -> Result<Option<Operation>, StoreError> {
    type Row = (
        i64,
        String,
        String,
        i64,
        i64,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<i64>,
    );
    let row: Option<Row> = tx
        .query_row(
            &format!(
                "SELECT id, state, service, account_id, started_at, reference, sealed, sign_in \
                 FROM provider_operations WHERE job_id = ?1 AND state IN {OPEN}"
            ),
            [sql(job)?],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((id, state, service, account, started_at, reference, sealed, sign_in)) = row else {
        return Ok(None);
    };
    let corrupt = || StoreError::Corrupt("provider operation");
    let sealed = match (reference, sealed) {
        (Some(reference), Some(sealed)) => {
            let reference: [u8; REFERENCE_BYTES] = reference.try_into().map_err(|_| corrupt())?;
            Some((reference, sealed))
        }
        (None, None) => None,
        _ => return Err(corrupt()),
    };
    Ok(Some(Operation {
        id,
        state: OperationState::from_str(&state).map_err(|_| corrupt())?,
        service: ProviderId::new(&service).map_err(|_| corrupt())?,
        account: key(account)?,
        started_at: UtcTimestamp::from_unix_millis(started_at),
        sealed,
        sign_in: sign_in.map(key).transpose()?,
    }))
}

/// Retires every succeeded operation of `job`, erasing its link: the job
/// completed or was cancelled, so nothing will use it.
pub(super) fn retire_links(tx: &Transaction<'_>, job: JobId) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE provider_operations SET state = 'retired', reference = NULL, sealed = NULL \
         WHERE job_id = ?1 AND state = 'succeeded'",
        [sql(job)?],
    )?;
    Ok(())
}

/// Retires every open operation through `account`, erasing their links:
/// the account was removed, and its key may not outlive it.
pub(super) fn retire_account(tx: &Transaction<'_>, account: AccountId) -> Result<(), StoreError> {
    tx.execute(
        &format!(
            "UPDATE provider_operations SET state = 'retired', reference = NULL, sealed = NULL \
             WHERE account_id = ?1 AND state IN {OPEN}"
        ),
        [sql(account)?],
    )?;
    Ok(())
}

/// Marks the job's ambiguous operation confirmed: the user accepted that
/// repeating it may repeat the provider's work. Returns whether the job
/// had one.
pub(super) fn confirm(tx: &Transaction<'_>, job: JobId) -> Result<bool, StoreError> {
    let changed = tx.execute(
        "UPDATE provider_operations SET state = 'confirmed' \
         WHERE job_id = ?1 AND state IN ('intended', 'ambiguous')",
        [sql(job)?],
    )?;
    Ok(changed == 1)
}

/// The job's route, if it was started through an account.
pub(super) fn route(tx: &Transaction<'_>, job: JobId) -> Result<Option<JobRoute>, StoreError> {
    let row: (Option<String>, Option<i64>) = tx.query_row(
        "SELECT route_service, route_account FROM jobs WHERE id = ?1",
        [sql(job)?],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    decode_route(row.0, row.1)
}

/// A stored route, both columns or neither.
pub(super) fn decode_route(
    service: Option<String>,
    account: Option<i64>,
) -> Result<Option<JobRoute>, StoreError> {
    match (service, account) {
        (Some(service), Some(account)) => Ok(Some(JobRoute {
            service: ProviderId::new(&service).map_err(|_| StoreError::Corrupt("job route"))?,
            account: key(account)?,
        })),
        (None, None) => Ok(None),
        _ => Err(StoreError::Corrupt("job route")),
    }
}

impl Store {
    /// The job's route and open operation.
    pub(crate) fn provider_operation(
        &mut self,
        job: JobId,
    ) -> Result<(Option<JobRoute>, Option<Operation>), StoreError> {
        let tx = self.read()?;
        Ok((route(&tx, job)?, open(&tx, job)?))
    }

    /// Records the intent to ask `job`'s provider for a link, before the
    /// request is sent, while the job is active at `generation`. Refused
    /// while the job has an open operation.
    pub(crate) fn begin_operation(
        &mut self,
        job: JobId,
        generation: Generation,
        route: &JobRoute,
        now: UtcTimestamp,
    ) -> Result<Operation, StoreError> {
        let tx = self.write()?;
        let stored = jobs::load(&tx, job)?;
        let current = stored.state.generation();
        if current != generation || !stored.state.stage().is_active() {
            return Err(StoreError::Job(
                wayfetch_domain::job::JobError::StaleGeneration {
                    current,
                    presented: generation,
                },
            ));
        }
        if open(&tx, job)?.is_some() {
            return Err(StoreError::Busy("the job already has a provider operation"));
        }
        tx.execute(
            "INSERT INTO provider_operations \
             (job_id, generation, service, account_id, state, started_at) \
             VALUES (?1, ?2, ?3, ?4, 'intended', ?5)",
            params![
                sql(job)?,
                sql(generation)?,
                route.service.as_str(),
                sql(route.account)?,
                now.unix_millis()
            ],
        )?;
        let operation = open(&tx, job)?.ok_or(StoreError::Corrupt("provider operation"))?;
        tx.commit()?;
        Ok(operation)
    }

    /// Records how operation `id` ended, if it is still waiting for its
    /// outcome (`intended` or `confirmed`). A link nothing will use, since
    /// the job ended or moved on (`keep` false), is recorded as retired
    /// and never stored. Returns whether it changed.
    pub(crate) fn finish_operation(
        &mut self,
        id: i64,
        outcome: &OperationOutcome,
        keep: bool,
        now: UtcTimestamp,
    ) -> Result<bool, StoreError> {
        let tx = self.write()?;
        let (state, reference, sealed, sign_in, problem) = match outcome {
            OperationOutcome::Succeeded {
                reference,
                sealed,
                sign_in,
            } if keep => (
                OperationState::Succeeded,
                Some(&reference[..]),
                Some(sealed.as_slice()),
                Some(sql(*sign_in)?),
                None,
            ),
            OperationOutcome::Succeeded { .. } => (OperationState::Retired, None, None, None, None),
            OperationOutcome::Failed(why) => (OperationState::Failed, None, None, None, Some(why)),
            OperationOutcome::Ambiguous(why) => {
                (OperationState::Ambiguous, None, None, None, Some(why))
            }
        };
        let state: &'static str = state.into();
        let changed = tx.execute(
            "UPDATE provider_operations SET state = ?2, reference = ?3, sealed = ?4, \
             sign_in = ?7, problem = ?5, finished_at = ?6 \
             WHERE id = ?1 AND state IN ('intended', 'confirmed')",
            params![
                id,
                state,
                reference,
                sealed,
                problem.map(|p| DiagnosticText::new(p).as_str().to_owned()),
                now.unix_millis(),
                sign_in
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// The original links of operations through `account`, other than
    /// `operation`, that may have generated a link at or after `since`:
    /// open ones, and any that ended since, except those that certainly
    /// generated nothing. Earlier operations of the same job count too.
    /// The newest 1000 at most.
    pub(crate) fn recent_operation_links(
        &mut self,
        account: AccountId,
        operation: i64,
        since: UtcTimestamp,
    ) -> Result<Vec<String>, StoreError> {
        let tx = self.read()?;
        let links = tx
            .prepare(
                "SELECT s.input_url FROM provider_operations p \
                 JOIN jobs j ON j.id = p.job_id JOIN source_links s ON s.id = j.source_link_id \
                 WHERE p.account_id = ?1 AND p.id <> ?2 AND p.state <> 'failed' \
                 AND (p.finished_at IS NULL OR p.finished_at >= ?3) \
                 ORDER BY p.id DESC LIMIT 1000",
            )?
            .query_map(
                params![sql(account)?, operation, since.unix_millis()],
                |r| r.get(0),
            )?
            .collect::<Result<_, _>>()?;
        Ok(links)
    }

    /// Retires operation `id`, erasing its link: the download host refused
    /// it, or a confirmed repeat found no record of it.
    pub(crate) fn retire_operation(
        &mut self,
        id: i64,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE provider_operations SET state = 'retired', reference = NULL, sealed = NULL, \
             finished_at = COALESCE(finished_at, ?2) WHERE id = ?1",
            params![id, now.unix_millis()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Jobs started through `account` that are resolving or transferring
    /// now.
    pub(crate) fn active_routed_jobs(
        &mut self,
        account: AccountId,
    ) -> Result<Vec<(JobId, Generation)>, StoreError> {
        let tx = self.read()?;
        let rows: Vec<(i64, i64)> = tx
            .prepare(
                "SELECT id, generation FROM jobs WHERE route_account = ?1 \
                 AND stage IN ('resolving', 'transferring') ORDER BY id",
            )?
            .query_map([sql(account)?], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|(id, generation)| Ok((key(id)?, key(generation)?)))
            .collect()
    }
}
