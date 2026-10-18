//! Explicit, retryable deletion of recorded artifacts. The store never touches files.
use super::commands::{record_as, replayed};
use super::{Store, StoreError, jobs, key, renames, sql};
use crate::fs::{FileIdentity, PublishedFile, RelativePath, StagingRecord};
use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

/// Recorded data authorized for deletion, never paths supplied by clients.
#[derive(Debug)]
pub struct DeleteIntent {
    /// Published or unfinished publication targets.
    pub published: Vec<(RootId, PublishedFile)>,
    /// Retained staging files, including older generations.
    pub staging: Vec<StagingRecord>,
}
/// A replayed result or a durable destructive intent.
#[derive(Debug)]
pub enum DeleteStart {
    /// Already completed.
    Done(CommandResult),
    /// Data to remove before finishing the command.
    Started(DeleteIntent),
}

pub(super) fn pending(tx: &Transaction<'_>, job: JobId) -> Result<bool, StoreError> {
    let Ok(id) = sql(job) else { return Ok(false) };
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deletions WHERE job_id = ?1)",
        [id],
        |r| r.get(0),
    )?)
}
pub(super) fn check_idle(tx: &Transaction<'_>, job: JobId) -> Result<(), StoreError> {
    if pending(tx, job)? {
        return Err(StoreError::Busy(
            "data deletion is unfinished; retry deletion or choose remove from list only",
        ));
    }
    Ok(())
}
pub(super) fn holding(
    tx: &Transaction<'_>,
    idempotency_key: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT fingerprint FROM deletions WHERE idempotency_key = ?1",
            [idempotency_key],
            |r| r.get(0),
        )
        .optional()?)
}

impl Store {
    /// Checks removal eligibility without changing the job, before a live
    /// task ownership refusal. Domain revision/stage errors keep precedence.
    pub(crate) fn check_remove_request(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
    ) -> Result<(), ProtocolError> {
        let tx = self.read()?;
        if replayed(&tx, envelope, instance)?.is_some() {
            return Ok(());
        }
        let (job, expected) = match envelope.command {
            Command::Remove {
                job,
                expected_revision,
            }
            | Command::DeleteFiles {
                job,
                expected_revision,
            } => (job, expected_revision),
            _ => return Err(StoreError::Corrupt("removal command").into()),
        };
        jobs::load_listed(&tx, job)?
            .state
            .check_remove(expected)
            .map_err(StoreError::from)?;
        Ok(())
    }

    /// Records explicit authorization before any file is touched. Retrying
    /// an intent resumes it; another command cannot reuse its key.
    pub fn begin_delete(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
    ) -> Result<DeleteStart, ProtocolError> {
        let Command::DeleteFiles {
            job: id,
            expected_revision,
        } = envelope.command
        else {
            return Err(StoreError::Corrupt("delete command").into());
        };
        let tx = self.write()?;
        if envelope.target != instance {
            return Err(ProtocolError::new(
                ErrorCode::WrongInstance,
                "command targets another engine instance",
            ));
        }
        let command_key = envelope.idempotency_key.as_uuid().to_string();
        // A matching pending key is resumed below; every other key still
        // follows normal replay/conflict checks before looking up the job.
        if let Some(fingerprint) = holding(&tx, &command_key)? {
            if fingerprint != envelope.command.fingerprint().as_bytes() {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "idempotency key was used for a different command",
                ));
            }
        } else if let Some(result) = replayed(&tx, envelope, instance)? {
            return Ok(DeleteStart::Done(result));
        }
        // Valid protocol IDs above SQLite's signed range cannot name a job.
        let stored_id = sql(id).map_err(|_| StoreError::NotFound("job"))?;
        let existing: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT idempotency_key, fingerprint FROM deletions WHERE job_id = ?1",
                [stored_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        if let Some((held_key, fingerprint)) = &existing
            && fingerprint != envelope.command.fingerprint().as_bytes()
        {
            return Err(ProtocolError::new(
                if held_key == &command_key {
                    ErrorCode::IdempotencyConflict
                } else {
                    ErrorCode::Busy
                },
                "an earlier deletion is unfinished; retry its request or remove from list only",
            ));
        }
        let job = jobs::load_listed(&tx, id)?;
        job.state
            .check_remove(expected_revision)
            .map_err(StoreError::from)?;
        if renames::pending(&tx, id)? {
            return Err(StoreError::Busy("the file is being renamed; try again").into());
        }
        if existing.is_none() {
            tx.execute(
                "INSERT INTO deletions (job_id, revision, generation, idempotency_key, fingerprint) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    sql(id)?,
                    sql(expected_revision)?,
                    sql(job.state.generation())?,
                    command_key,
                    envelope.command.fingerprint().as_bytes().as_slice(),
                ],
            ).map_err(StoreError::from)?;
        }
        let published = tx
            .prepare(
                "SELECT root_id, target_path, file_device, file_inode, file_birth_ns, size_bytes \
             FROM publications WHERE job_id = ?1",
            )
            .map_err(StoreError::from)?
            .query_map([sql(id)?], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(StoreError::from)?
            .map(|row| {
                let (root, path, device, inode, birth_ns, size) = row?;
                Ok((
                    key(root)?,
                    PublishedFile {
                        path: RelativePath::new(&path)
                            .map_err(|_| StoreError::Corrupt("publication path"))?,
                        identity: FileIdentity {
                            device: device.cast_unsigned(),
                            inode: inode.cast_unsigned(),
                            birth_ns,
                        },
                        size: u64::try_from(size)
                            .map_err(|_| StoreError::Corrupt("publication size"))?,
                    },
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let staging = tx
            .prepare(
                "SELECT root_id, generation, name, file_device, file_inode, file_birth_ns \
             FROM staging_files WHERE job_id = ?1",
            )
            .map_err(StoreError::from)?
            .query_map([sql(id)?], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })
            .map_err(StoreError::from)?
            .map(|row| {
                let (root, generation, name, device, inode, birth_ns) = row?;
                Ok(StagingRecord {
                    root: key(root)?,
                    job: id,
                    generation: key(generation)?,
                    name,
                    identity: FileIdentity {
                        device: device.cast_unsigned(),
                        inode: inode.cast_unsigned(),
                        birth_ns,
                    },
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let mut staging = staging;
        staging.extend(super::media::aux_files(&tx, id)?);
        tx.commit().map_err(StoreError::from)?;
        Ok(DeleteStart::Started(DeleteIntent { published, staging }))
    }

    /// Hides the job and frees artifact reservations only after every file
    /// removal and directory sync succeeded. Original command replay is atomic.
    pub fn finish_delete(
        &mut self,
        id: JobId,
        envelope: &CommandEnvelope,
        now: UtcTimestamp,
    ) -> Result<CommandResult, StoreError> {
        let tx = self.write()?;
        let (revision, generation, command_key, fingerprint): (i64, i64, String, Vec<u8>) = tx
            .query_row(
                "SELECT revision, generation, idempotency_key, fingerprint \
                 FROM deletions WHERE job_id = ?1",
                [sql(id)?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let job = jobs::load_listed(&tx, id)?;
        job.state.check_remove(key(revision)?)?;
        if sql(job.state.generation())? != generation {
            return Err(StoreError::Corrupt("deletion generation"));
        }
        tx.execute("DELETE FROM publications WHERE job_id=?1", [sql(id)?])?;
        tx.execute("DELETE FROM partials WHERE job_id=?1", [sql(id)?])?;
        tx.execute("DELETE FROM staging_files WHERE job_id=?1", [sql(id)?])?;
        tx.execute("DELETE FROM deletions WHERE job_id=?1", [sql(id)?])?;
        let result = super::commands::remove(&tx, id, key(revision)?, now)?;
        record_as(&tx, &command_key, &fingerprint, &result, now)?;
        if envelope.idempotency_key.as_uuid().to_string() != command_key {
            super::commands::record(&tx, envelope, &result, now)?;
        }
        tx.commit()?;
        Ok(result)
    }
}
