//! Renaming published files (ADR-16, ADR-17).
//!
//! A rename is recorded before the file gets its new name and finished
//! after, like a publication: [`Store::begin_rename`] checks the request
//! and reserves the new name, the filesystem links it
//! ([`crate::fs::Root::rename_published`]), and [`Store::finish_rename`]
//! records it on the publication with the job's new revision and the
//! command's idempotency record. The record keeps the command's key and
//! fingerprint, so a rename a crash interrupted is finished after the
//! restart ([`crate::fs::Root::reconcile_rename`]) with the same result a
//! retry then replays.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

use super::commands::{record, record_as, replayed};
use super::{OrCorrupt, Store, StoreError, jobs, key, sql};
use crate::fs::{FileIdentity, PathError, PublishedFile, RelativePath};

/// A rename recorded but not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameIntent {
    /// The job.
    pub job: JobId,
    /// The root holding its file.
    pub root: RootId,
    /// The file, at its current name.
    pub file: PublishedFile,
    /// The new name.
    pub to: RelativePath,
}

/// What [`Store::begin_rename`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameStart {
    /// The command is answered: replayed, or the file already has the name.
    Done(CommandResult),
    /// The rename is recorded; rename the file, then finish or abandon it.
    Started(RenameIntent),
}

/// Whether a rename of `job`'s file is recorded and unfinished.
pub(super) fn pending(tx: &Transaction<'_>, job: JobId) -> Result<bool, StoreError> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM renames WHERE job_id = ?1",
            [sql(job)?],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The command fingerprint of the unfinished rename recorded under
/// `idempotency_key`, if any.
pub(super) fn holding(
    tx: &Transaction<'_>,
    idempotency_key: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT fingerprint FROM renames WHERE idempotency_key = ?1",
            [idempotency_key],
            |row| row.get(0),
        )
        .optional()?)
}

/// The publication row of `job`'s published file, its root, and the file.
pub(super) fn published(
    tx: &Transaction<'_>,
    job: JobId,
) -> Result<Option<(i64, RootId, PublishedFile)>, StoreError> {
    let row = tx
        .query_row(
            "SELECT id, root_id, target_path, file_device, file_inode, file_birth_ns, size_bytes \
             FROM publications WHERE job_id = ?1 AND state = 'published'",
            [sql(job)?],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .optional()?;
    row.map(|(id, root, path, device, inode, birth_ns, size)| {
        Ok((
            id,
            key(root)?,
            PublishedFile {
                path: RelativePath::new(&path).or_corrupt("publication target")?,
                identity: FileIdentity {
                    device: device.cast_unsigned(),
                    inode: inode.cast_unsigned(),
                    birth_ns,
                },
                size: u64::try_from(size).or_corrupt("size")?,
            },
        ))
    })
    .transpose()
}

/// `name` in the folder of `path`, checked by the filename policy. Only a
/// name is accepted, never a path to another folder.
fn renamed(path: &RelativePath, name: &str) -> Result<RelativePath, StoreError> {
    // An empty name would otherwise name the folder.
    if name.is_empty() {
        return Err(StoreError::InvalidName(PathError::Empty));
    }
    if name.contains('/') {
        return Err(StoreError::InvalidName(PathError::ForbiddenChar('/')));
    }
    let target = match path.split_last() {
        (Some(parent), _) => format!("{parent}/{name}"),
        (None, _) => name.to_owned(),
    };
    RelativePath::new(&target).map_err(StoreError::InvalidName)
}

impl Store {
    /// Checks a rename command and records it, reserving the new name. The
    /// job must be listed, complete at the revision the client saw, and not
    /// already being renamed; the name must pass the filename policy, must
    /// not differ from the old one only in case, and no other publication
    /// or rename in the root may hold its case-folded form.
    pub fn begin_rename(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
        now: UtcTimestamp,
    ) -> Result<RenameStart, ProtocolError> {
        let Command::Rename {
            job: id,
            expected_revision,
            name,
        } = &envelope.command
        else {
            return Err(StoreError::Corrupt("rename command").into());
        };
        let tx = self.write()?;
        if let Some(result) = replayed(&tx, envelope, instance)? {
            return Ok(RenameStart::Done(result));
        }
        super::deletions::check_idle(&tx, *id)?;
        super::extractions::check_idle(&tx, *id)?;
        let job = jobs::load_listed(&tx, *id)?;
        // Checks the revision and stage without keeping the new revision:
        // that is recorded once the file has its name.
        let mut state = job.state;
        state.request_rename(*expected_revision)?;
        if pending(&tx, *id)? {
            return Err(StoreError::Busy("the file is already being renamed").into());
        }
        let (publication, root, file) = published(&tx, *id)?.or_corrupt("published file")?;
        let to = renamed(&file.path, name)?;
        if to == file.path {
            let result = CommandResult::JobUpdated {
                job: *id,
                revision: job.state.revision(),
                changed: false,
            };
            record(&tx, envelope, &result, now)?;
            tx.commit().map_err(StoreError::from)?;
            return Ok(RenameStart::Done(result));
        }
        // On a case-insensitive filesystem both names open one entry, which
        // a rename would remove; changing only the case is refused.
        if to.collision_key() == file.path.collision_key() {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "a new name that differs only in letter case is not supported",
            ));
        }
        let taken: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM publications \
                     WHERE root_id = ?1 AND target_key = ?2 AND id != ?3) \
                 OR EXISTS (SELECT 1 FROM renames WHERE root_id = ?1 AND target_key = ?2)",
                params![sql(root)?, to.collision_key(), publication],
                |row| row.get(0),
            )
            .map_err(StoreError::from)?;
        if taken {
            return Err(StoreError::TargetReserved(to).into());
        }
        tx.execute(
            "INSERT INTO renames (job_id, publication_id, root_id, target_path, target_key, \
             idempotency_key, fingerprint, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                sql(*id)?,
                publication,
                sql(root)?,
                to.as_str(),
                to.collision_key(),
                envelope.idempotency_key.as_uuid().to_string(),
                envelope.command.fingerprint().as_bytes().as_slice(),
                now.unix_millis(),
            ],
        )
        .map_err(StoreError::from)?;
        tx.commit().map_err(StoreError::from)?;
        Ok(RenameStart::Started(RenameIntent {
            job: *id,
            root,
            file,
            to,
        }))
    }

    /// Records the job's file at the name its rename gave it, advances the
    /// job's revision with a `job_changed` event, and records the command's
    /// result, in one transaction. Call it only after the filesystem
    /// confirmed the file at its new name.
    pub fn finish_rename(
        &mut self,
        id: JobId,
        now: UtcTimestamp,
    ) -> Result<CommandResult, StoreError> {
        let tx = self.write()?;
        let (publication, path, target_key, idempotency_key, fingerprint): (
            i64,
            String,
            String,
            String,
            Vec<u8>,
        ) = tx
            .query_row(
                "SELECT publication_id, target_path, target_key, idempotency_key, fingerprint \
                 FROM renames WHERE job_id = ?1",
                [sql(id)?],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound("rename"))?;
        tx.execute("DELETE FROM renames WHERE job_id = ?1", [sql(id)?])?;
        tx.execute(
            "UPDATE publications SET target_path = ?1, target_key = ?2 WHERE id = ?3",
            params![path, target_key, publication],
        )?;
        // Loaded after the new name is recorded, so the event carries it.
        let mut job = jobs::load(&tx, id)?;
        let previous = job.state;
        job.state.request_rename(previous.revision())?;
        jobs::save(&tx, &mut job, &previous, None, now)?;
        let result = CommandResult::JobUpdated {
            job: id,
            revision: job.state.revision(),
            changed: true,
        };
        record_as(&tx, &idempotency_key, &fingerprint, &result, now)?;
        tx.commit()?;
        Ok(result)
    }

    /// Drops the job's unfinished rename, releasing the new name, when the
    /// file was not renamed. No command is recorded, so the client can
    /// retry under the same key.
    pub fn abandon_rename(&mut self, id: JobId) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute("DELETE FROM renames WHERE job_id = ?1", [sql(id)?])?;
        tx.commit()?;
        Ok(())
    }

    /// Every unfinished rename, which a restart reconciles.
    pub fn pending_renames(&mut self) -> Result<Vec<RenameIntent>, StoreError> {
        self.renames_where("1", None)
    }

    /// The job's unfinished rename, if it has one.
    pub fn pending_rename(&mut self, job: JobId) -> Result<Option<RenameIntent>, StoreError> {
        Ok(self.renames_where("job_id = ?1", Some(job))?.pop())
    }

    /// The unfinished renames matching `condition` on `job`.
    fn renames_where(
        &mut self,
        condition: &str,
        job: Option<JobId>,
    ) -> Result<Vec<RenameIntent>, StoreError> {
        let tx = self.read()?;
        let job = job.map(sql).transpose()?;
        let rows: Vec<(i64, String)> = tx
            .prepare(&format!(
                "SELECT job_id, target_path FROM renames WHERE {condition} ORDER BY job_id"
            ))?
            .query_map(rusqlite::params_from_iter(job), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        rows.into_iter()
            .map(|(job, to)| {
                let job = key(job)?;
                let (_, root, file) = published(&tx, job)?.or_corrupt("published file")?;
                Ok(RenameIntent {
                    job,
                    root,
                    file,
                    to: RelativePath::new(&to).or_corrupt("rename target")?,
                })
            })
            .collect()
    }

    /// The listed job's published file and the root holding it, for a host
    /// to open on its user's request. `NotFound` until the job is complete;
    /// `Busy` while its file is being renamed.
    pub fn published_file(&mut self, id: JobId) -> Result<(RootId, PublishedFile), StoreError> {
        let tx = self.read()?;
        super::deletions::check_idle(&tx, id)?;
        jobs::load_listed(&tx, id)?;
        if pending(&tx, id)? {
            return Err(StoreError::Busy("the file is being renamed"));
        }
        let (_, root, file) = published(&tx, id)?.ok_or(StoreError::NotFound("published file"))?;
        Ok((root, file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_stay_in_their_folder() {
        let path = RelativePath::new("Movies/a.bin").unwrap();
        assert_eq!(renamed(&path, "b.bin").unwrap().as_str(), "Movies/b.bin");
        let top = RelativePath::new("a.bin").unwrap();
        assert_eq!(renamed(&top, "b.bin").unwrap().as_str(), "b.bin");
        for bad in [
            "",
            "..",
            ".",
            "../b.bin",
            "x/b.bin",
            "b\\c",
            "con",
            "b.",
            ".wayfetch",
        ] {
            assert!(
                matches!(renamed(&top, bad), Err(StoreError::InvalidName(_))),
                "{bad:?} accepted"
            );
        }
        // Inside a folder, the private name is only a name.
        assert!(renamed(&path, ".wayfetch").is_ok());
        let long = "x".repeat(256);
        assert!(renamed(&top, &long).is_err());
    }
}
