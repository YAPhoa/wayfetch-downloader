//! Durable, idempotent client commands (architecture brief sections 6.2
//! and 11.1; test T46).
//!
//! A command, its outbox events, and its idempotency record commit in one
//! transaction, and the result is returned only after that commit. A retry
//! under the same key replays the stored result without changing anything;
//! the same key with a different command is rejected. A command that fails
//! is rolled back and leaves no record, so its key can be retried.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, PackageId};
use wayfetch_domain::job::{JobError, JobState, Outcome};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::command::{CommandFingerprint, IdempotencyRecord, Replay, SubmitItems};
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

use super::{Store, StoreError, jobs, key, outbox, sql};

impl Store {
    /// Applies a client command durably. The host has already checked the
    /// caller's permission.
    pub fn execute(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
        now: UtcTimestamp,
    ) -> Result<CommandResult, ProtocolError> {
        if envelope.target != instance {
            return Err(ProtocolError::new(
                ErrorCode::WrongInstance,
                "command targets another engine instance",
            ));
        }
        let idempotency_key = envelope.idempotency_key.as_uuid().to_string();
        let tx = self.write()?;
        if let Some(record) = stored(&tx, &idempotency_key)? {
            return match record.replay(&envelope.command) {
                Replay::Stored(result) => Ok(result.clone()),
                Replay::Conflict => Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "idempotency key was used for a different command",
                )),
            };
        }
        let result = match &envelope.command {
            Command::Submit { items, package } => submit(&tx, items, *package, now)?,
            Command::Pause {
                job,
                expected_revision,
            } => request(&tx, *job, now, |s| s.request_pause(*expected_revision))?,
            Command::Resume {
                job,
                expected_revision,
            } => request(&tx, *job, now, |s| s.request_resume(*expected_revision))?,
            Command::Cancel {
                job,
                expected_revision,
            } => request(&tx, *job, now, |s| s.request_cancel(*expected_revision))?,
        };
        let result_json =
            serde_json::to_string(&result).map_err(|_| StoreError::Corrupt("command result"))?;
        tx.execute(
            "INSERT INTO commands (idempotency_key, fingerprint, result, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                idempotency_key,
                envelope.command.fingerprint().as_bytes().as_slice(),
                result_json,
                now.unix_millis(),
            ],
        )
        .map_err(StoreError::from)?;
        tx.commit().map_err(StoreError::from)?;
        Ok(result)
    }
}

fn stored(tx: &Transaction<'_>, key: &str) -> Result<Option<IdempotencyRecord>, StoreError> {
    let Some((fingerprint, result)) = tx
        .query_row(
            "SELECT fingerprint, result FROM commands WHERE idempotency_key = ?1",
            [key],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let fingerprint: [u8; 32] = fingerprint
        .try_into()
        .map_err(|_| StoreError::Corrupt("command fingerprint"))?;
    Ok(Some(IdempotencyRecord {
        fingerprint: CommandFingerprint::from_bytes(fingerprint),
        result: serde_json::from_str(&result).map_err(|_| StoreError::Corrupt("command result"))?,
    }))
}

/// Records a batch and its links for the ingestion workflow. Jobs are
/// created by [`Store::stage_pending`], not here: the result is the durable
/// batch.
fn submit(
    tx: &Transaction<'_>,
    items: &SubmitItems,
    package: Option<PackageId>,
    now: UtcTimestamp,
) -> Result<CommandResult, StoreError> {
    let package = match package {
        Some(id) => {
            let id = sql(id).map_err(|_| StoreError::NotFound("package"))?;
            tx.query_row("SELECT id FROM packages WHERE id = ?1", [id], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .ok_or(StoreError::NotFound("package"))?
        }
        None => {
            // Name a new package after the first input's host.
            let name = items.as_slice()[0].as_url().host_str().unwrap_or("Package");
            tx.execute(
                "INSERT INTO packages (name, created_at) VALUES (?1, ?2)",
                params![name, now.unix_millis()],
            )?;
            tx.last_insert_rowid()
        }
    };
    let accepted =
        u32::try_from(items.as_slice().len()).map_err(|_| StoreError::Corrupt("input count"))?;
    tx.execute(
        "INSERT INTO input_batches (package_id, input_count, created_at) VALUES (?1, ?2, ?3)",
        params![package, accepted, now.unix_millis()],
    )?;
    let batch = key(tx.last_insert_rowid())?;
    let mut insert =
        tx.prepare("INSERT INTO source_links (batch_id, ordinal, input_url) VALUES (?1, ?2, ?3)")?;
    for (ordinal, item) in items.as_slice().iter().enumerate() {
        insert.execute(params![
            sql(batch)?,
            i64::try_from(ordinal).map_err(|_| StoreError::Corrupt("ordinal"))?,
            item.as_url().as_str()
        ])?;
    }
    outbox::append(tx, &EventKind::BatchAccepted { batch, accepted }, now)?;
    Ok(CommandResult::Submitted { batch, accepted })
}

/// Applies a revision-checked user request to one job.
fn request(
    tx: &Transaction<'_>,
    id: JobId,
    now: UtcTimestamp,
    apply: impl FnOnce(&mut JobState) -> Result<Outcome, JobError>,
) -> Result<CommandResult, StoreError> {
    let mut job = jobs::load(tx, id)?;
    let previous = job.state;
    let outcome = apply(&mut job.state)?;
    if outcome == Outcome::Changed {
        let failure = job.failure.take();
        jobs::save(tx, &mut job, &previous, failure, now)?;
    }
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: outcome == Outcome::Changed,
    })
}
