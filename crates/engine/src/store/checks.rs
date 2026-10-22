//! Checking complete downloads against checksums, and downloading again a
//! file that failed (see `docs/architecture.md`, "Amendment (file
//! checks)").
//!
//! A checksum file downloaded in a batch is read once; its entries refer
//! to the batch's other downloads by name, never to paths to open. Each
//! check is bound to the published file it is about and the source of its
//! value. A file that failed is downloaded again through a durable
//! replacement: authorized, the file removed while unchanged, then the job
//! queued under a new generation.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, PackageId, RootId};
use wayfetch_domain::name::DiagnosticText;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::query::{FileCheck, FileCheckState};
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

use super::commands::{record_as, replayed};
use super::{OrCorrupt, Store, StoreError, deletions, extractions, jobs, key, renames, sql};
use crate::checksum::manifest::{Kind, Manifest};
use crate::checksum::{Algorithm, Digest};
use crate::fs::{FileVersion, PublishedFile};

/// Most times a job is downloaded again on its own after failing a check.
pub const MAX_CHECK_REDOWNLOADS: i64 = 1;

/// Separates the fields of a job's displayed check in its query column
/// (`char(31)` in SQL).
const FIELD: char = '\u{1f}';

/// A job's check from its query columns: the check that matters most
/// for the file it published, its fields joined by [`FIELD`], and whether
/// the file is being replaced.
pub(super) fn decode(
    row: Option<String>,
    replacing: bool,
) -> Result<Option<FileCheck>, StoreError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let mut fields = row.split(FIELD);
    let mut next = || fields.next().or_corrupt("check");
    let (state, algorithm, source, problem) = (next()?, next()?, next()?, next()?);
    let state = if replacing {
        FileCheckState::Replacing
    } else {
        FileCheckState::from_str(state).or_corrupt("check state")?
    };
    Ok(Some(FileCheck {
        state,
        algorithm: Algorithm::from_str(algorithm).or_corrupt("check algorithm")?,
        source: (!source.is_empty()).then(|| source.to_owned()),
        problem: (!problem.is_empty()).then(|| DiagnosticText::new(problem)),
    }))
}

/// A replacement's publication, generation, reason, and the command's
/// idempotency key and fingerprint, if a person asked for it.
type ReplacementRow = (i64, i64, String, Option<String>, Option<Vec<u8>>);

/// A published file to check, as the store records it.
#[derive(Debug, Clone)]
pub(crate) struct CheckPlan {
    /// The publication it is.
    pub(crate) publication: i64,
    pub(crate) root: RootId,
    pub(crate) file: PublishedFile,
    /// When it was modified as seen right after publishing, if seen.
    pub(crate) modified_ns: Option<i64>,
    /// The hashes its pending checks need.
    pub(crate) algorithms: Vec<Algorithm>,
}

/// How reading a file for its check ended.
#[derive(Debug, Clone)]
pub(crate) enum CheckOutcome {
    /// It was read, unchanged throughout: its version and hashes.
    Hashed(FileVersion, Vec<Digest>),
    /// It is gone, or was changed or replaced outside Wayfetch.
    Changed(&'static str),
}

/// A checksum file to read.
#[derive(Debug, Clone)]
pub(crate) struct ManifestPlan {
    pub(crate) job: JobId,
    pub(crate) publication: i64,
    pub(crate) root: RootId,
    pub(crate) file: PublishedFile,
    pub(crate) modified_ns: Option<i64>,
    pub(crate) kind: Kind,
}

/// A file that failed its check, authorized to be removed so its job
/// downloads it again.
#[derive(Debug, Clone)]
pub(crate) struct Replacement {
    pub(crate) job: JobId,
    pub(crate) root: RootId,
    pub(crate) file: PublishedFile,
    /// The modification time the failing check saw, if known.
    pub(crate) modified_ns: Option<i64>,
}

/// What [`Store::begin_replacement`] decided.
#[derive(Debug, Clone)]
pub(crate) enum ReplaceStart {
    /// The command is answered: replayed.
    Done(CommandResult),
    /// The replacement is recorded; remove the file, then finish it.
    Started(Replacement),
}

/// The publication row of `job`'s published file and its recorded
/// modification time.
fn modified(tx: &Transaction<'_>, publication: i64) -> Result<Option<i64>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT file_modified_ns FROM publications WHERE id = ?1",
            [publication],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// Whether complete downloads are checked.
fn checking(tx: &Transaction<'_>) -> Result<bool, StoreError> {
    Ok(super::roots::settings(tx)?.check_files)
}

/// Whether `name` is a checksum file's.
fn checksum_name(path: &str) -> Option<Kind> {
    Kind::of(path.rsplit('/').next()?)
}

/// The folder of a path below a root, with a trailing `/`, or empty.
fn folder(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..=i])
}

/// A complete, listed download of a package, published in a root.
struct Published {
    job: JobId,
    path: String,
    key: String,
    /// Where its unnumbered name would put it.
    intended: Option<String>,
}

/// The complete, listed downloads of `package` published in `root`, but
/// checksum files.
fn package_files(
    tx: &Transaction<'_>,
    package: PackageId,
    root: RootId,
) -> Result<Vec<Published>, StoreError> {
    let ids: Vec<i64> = tx
        .prepare(
            "SELECT j.id FROM jobs j JOIN publications a ON a.job_id = j.id \
             AND a.state = 'published' WHERE j.package_id = ?1 AND a.root_id = ?2 \
             AND j.removed_at IS NULL AND j.stage = 'complete'",
        )?
        .query_map(params![sql(package)?, sql(root)?], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut files = Vec::with_capacity(ids.len());
    for id in ids {
        let job = jobs::load(tx, key(id)?)?;
        let Some(path) = job.artifact_path() else {
            continue;
        };
        if checksum_name(path).is_some() {
            continue;
        }
        let intended = job
            .intended_name()
            .map(|name| format!("{}{name}", folder(path)));
        files.push(Published {
            job: job.id,
            path: path.to_owned(),
            key: path.to_lowercase(),
            intended,
        });
    }
    Ok(files)
}

/// An entry of a checksum file read in a package.
struct Listed {
    source: i64,
    source_name: String,
    path: String,
    key: String,
    digest: Digest,
}

/// Entries of the checksum files read in `package` and still published in
/// `root` whose case-folded path is one of `keys`.
fn entries(
    tx: &Transaction<'_>,
    package: PackageId,
    root: RootId,
    keys: &[&str],
) -> Result<Vec<Listed>, StoreError> {
    let mut stmt = tx.prepare(
        "SELECT e.publication_id, f.name, e.path, e.path_key, e.algorithm, e.value \
         FROM checksum_entries e JOIN checksum_files f ON f.publication_id = e.publication_id \
         JOIN publications p ON p.id = f.publication_id AND p.state = 'published' \
         WHERE f.package_id = ?1 AND f.root_id = ?2 AND f.state = 'read' AND e.path_key = ?3 \
         ORDER BY e.publication_id, e.seq",
    )?;
    let mut listed = Vec::new();
    for wanted in keys.iter().collect::<BTreeSet<_>>() {
        let rows = stmt.query_map(params![sql(package)?, sql(root)?, wanted], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        for row in rows {
            let (source, source_name, path, key, algorithm, value) = row?;
            let algorithm = Algorithm::from_str(&algorithm).or_corrupt("entry algorithm")?;
            listed.push(Listed {
                source,
                source_name,
                path,
                key,
                digest: Digest::parse(algorithm, &value).or_corrupt("entry value")?,
            });
        }
    }
    Ok(listed)
}

/// Whether `entry` names `file` among `files`: by its exact path; else by
/// its unnumbered name, if no download has that path and exactly one would;
/// else ignoring case, if no download claims the entry exactly and only
/// this one matches it.
fn names(entry: &Listed, file: &Published, files: &[Published]) -> bool {
    if entry.path == file.path {
        return true;
    }
    let exact = files.iter().any(|f| f.path == entry.path);
    let intending = files
        .iter()
        .filter(|f| f.intended.as_deref() == Some(&entry.path))
        .count();
    if file.intended.as_deref() == Some(&entry.path) {
        return !exact && intending == 1;
    }
    entry.key == file.key
        && !exact
        && intending == 0
        && files.iter().filter(|f| f.key == entry.key).count() == 1
}

/// Records pending checks of `job`'s published file against the entries
/// of its package's checksum files that name it. Entries of one source
/// that disagree, or sources that disagree, are a conflict, which is never
/// checked or downloaded again.
fn match_job(tx: &Transaction<'_>, id: JobId, now: UtcTimestamp) -> Result<(), StoreError> {
    let job = jobs::load(tx, id)?;
    if job.removed || job.state.stage() != wayfetch_domain::job::JobStage::Complete {
        return Ok(());
    }
    let Some((publication, root, file)) = renames::published(tx, id)? else {
        return Ok(());
    };
    if checksum_name(file.path.as_str()).is_some() {
        return Ok(());
    }
    let files = package_files(tx, job.package(), root)?;
    let Some(this) = files.iter().find(|f| f.job == id) else {
        return Ok(());
    };
    let intended_key = this.intended.as_deref().map(str::to_lowercase);
    let mut keys = vec![this.key.as_str()];
    keys.extend(intended_key.as_deref());
    let mut found: BTreeMap<(i64, Algorithm), (String, BTreeSet<Digest>)> = BTreeMap::new();
    for entry in entries(tx, job.package(), root, &keys)? {
        if names(&entry, this, &files) {
            found
                .entry((entry.source, entry.digest.algorithm()))
                .or_insert_with(|| (entry.source_name.clone(), BTreeSet::new()))
                .1
                .insert(entry.digest);
        }
    }
    let mut added = false;
    for ((source, algorithm), (source_name, values)) in &found {
        let (state, problem) = if values.len() > 1 {
            (
                "conflict",
                Some("The checksum file lists different values for this file."),
            )
        } else {
            ("pending", None)
        };
        let expected = values.first().or_corrupt("entry value")?;
        added |= tx.execute(
            "INSERT OR IGNORE INTO verifications (publication_id, source_publication_id, \
             algorithm, job_id, generation, source_name, expected, state, problem) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                publication,
                source,
                <&str>::from(*algorithm),
                sql(id)?,
                sql(job.state.generation())?,
                source_name,
                expected.value(),
                state,
                problem,
            ],
        )? == 1;
    }
    if added {
        tx.execute(
            "UPDATE verifications SET state = 'conflict', \
             problem = 'Checksum files list different values for this file.' \
             WHERE publication_id = ?1 AND state = 'pending' AND algorithm IN ( \
                 SELECT algorithm FROM verifications WHERE publication_id = ?1 \
                 GROUP BY algorithm HAVING COUNT(DISTINCT expected) > 1)",
            [publication],
        )?;
        jobs::announce(tx, id, now)?;
    }
    Ok(())
}

/// Whether `publication` has a check of `state`.
fn has_check(tx: &Transaction<'_>, publication: i64, state: &str) -> Result<bool, StoreError> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM verifications WHERE publication_id = ?1 AND state = ?2)",
        params![publication, state],
        |r| r.get(0),
    )?)
}

/// Whether `job`'s file is being replaced.
pub(super) fn pending(tx: &Transaction<'_>, job: JobId) -> Result<bool, StoreError> {
    let Ok(id) = sql(job) else { return Ok(false) };
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM replacements WHERE job_id = ?1)",
        [id],
        |r| r.get(0),
    )?)
}

/// Refuses a change to `job`'s file while it is being replaced.
pub(super) fn check_idle(tx: &Transaction<'_>, job: JobId) -> Result<(), StoreError> {
    if pending(tx, job)? {
        return Err(StoreError::Busy(
            "the file failed its check and is being downloaded again; try again shortly",
        ));
    }
    Ok(())
}

/// The command fingerprint of the unfinished replacement recorded under
/// `idempotency_key`, if any.
pub(super) fn holding(
    tx: &Transaction<'_>,
    idempotency_key: &str,
) -> Result<Option<Vec<u8>>, StoreError> {
    Ok(tx
        .query_row(
            "SELECT fingerprint FROM replacements WHERE idempotency_key = ?1",
            [idempotency_key],
            |r| r.get(0),
        )
        .optional()?)
}

/// What keeps an archive whose volumes are `parts` (with their jobs) from
/// being unpacked yet, if anything, while files are checked: a checksum
/// file of its package still downloading or not read yet, a part still
/// being checked, or a part that failed its check or is being replaced. A
/// checksum file that failed, was cancelled, or was removed holds nothing,
/// and the archive is unpacked unchecked; a conflict or change holds
/// nothing either, since no download can resolve it.
pub(super) fn extraction_hold(
    tx: &Transaction<'_>,
    package: PackageId,
    parts: &[JobId],
) -> Result<Option<&'static str>, StoreError> {
    if !checking(tx)? {
        return Ok(None);
    }
    let unfinished: Vec<i64> = tx
        .prepare(
            "SELECT id FROM jobs WHERE package_id = ?1 AND removed_at IS NULL \
             AND stage NOT IN ('complete', 'failed', 'cancelled')",
        )?
        .query_map([sql(package)?], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    for id in unfinished {
        if jobs::load(tx, key(id)?)?
            .intended_name()
            .is_some_and(|name| Kind::of(name).is_some())
        {
            return Ok(Some("Waiting for the checksum file to download."));
        }
    }
    let unread: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM checksum_files f JOIN jobs j ON j.id = f.job_id \
         WHERE f.package_id = ?1 AND f.state = 'pending' AND j.removed_at IS NULL)",
        [sql(package)?],
        |r| r.get(0),
    )?;
    if unread {
        return Ok(Some("Waiting for the checksum file to be read."));
    }
    for &part in parts {
        if pending(tx, part)? {
            return Ok(Some(
                "A part failed its check and is being downloaded again.",
            ));
        }
        let Some((publication, _, _)) = renames::published(tx, part)? else {
            continue;
        };
        if has_check(tx, publication, "failed")? {
            return Ok(Some(
                "A part failed its check; download it again to continue.",
            ));
        }
        if has_check(tx, publication, "pending")? {
            return Ok(Some("Checking the archive's files."));
        }
    }
    Ok(None)
}

/// Forgets what a checksum file at `publication` listed, and the checks
/// waiting on it.
fn forget_checksum_file(tx: &Transaction<'_>, publication: i64) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM checksum_entries WHERE publication_id = ?1",
        [publication],
    )?;
    tx.execute(
        "DELETE FROM verifications WHERE source_publication_id = ?1 AND state = 'pending'",
        [publication],
    )?;
    Ok(())
}

/// Drops what a removed publication's checks and checksum file recorded.
pub(super) fn forget_publication(tx: &Transaction<'_>, publication: i64) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM verifications WHERE publication_id = ?1",
        [publication],
    )?;
    tx.execute(
        "DELETE FROM checksum_files WHERE publication_id = ?1",
        [publication],
    )?;
    forget_checksum_file(tx, publication)
}

impl Store {
    /// After `job`'s download ended complete, with checks on: a checksum
    /// file is queued to be read, and any other file gets the checks its
    /// package's checksum files name it in.
    pub(crate) fn plan_checks(&mut self, job: JobId, now: UtcTimestamp) -> Result<(), StoreError> {
        let tx = self.write()?;
        if !checking(&tx)? {
            return Ok(());
        }
        let stored = jobs::load(&tx, job)?;
        let published = renames::published(&tx, job)?;
        if let (false, Some((publication, root, file))) = (stored.removed, published)
            && checksum_name(file.path.as_str()).is_some()
        {
            let previous: Option<i64> = tx
                .query_row(
                    "SELECT publication_id FROM checksum_files WHERE job_id = ?1",
                    [sql(job)?],
                    |r| r.get(0),
                )
                .optional()?;
            if previous != Some(publication) {
                if let Some(previous) = previous {
                    forget_checksum_file(&tx, previous)?;
                }
                let (_, name) = file.path.split_last();
                tx.execute(
                    "INSERT INTO checksum_files (job_id, publication_id, package_id, root_id, \
                     name, state, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6) \
                     ON CONFLICT (job_id) DO UPDATE SET publication_id = ?2, package_id = ?3, \
                     root_id = ?4, name = ?5, state = 'pending', skipped = 0, problem = NULL, \
                     updated_at = ?6",
                    params![
                        sql(job)?,
                        publication,
                        sql(stored.package())?,
                        sql(root)?,
                        name,
                        now.unix_millis()
                    ],
                )?;
            }
        } else {
            match_job(&tx, job, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Checksum files waiting to be read, up to `limit`, but those of
    /// `excluded` jobs.
    pub(crate) fn pending_checksum_files(
        &mut self,
        limit: usize,
        excluded: &[JobId],
    ) -> Result<Vec<ManifestPlan>, StoreError> {
        let tx = self.read()?;
        if !checking(&tx)? {
            return Ok(Vec::new());
        }
        let rows: Vec<(i64, i64)> = tx
            .prepare(
                "SELECT f.job_id, f.publication_id FROM checksum_files f \
                 JOIN jobs j ON j.id = f.job_id WHERE f.state = 'pending' \
                 AND j.removed_at IS NULL ORDER BY f.updated_at, f.job_id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut plans = Vec::new();
        for (job, publication) in rows {
            let job: JobId = key(job)?;
            if excluded.contains(&job) || renames::pending(&tx, job)? {
                continue;
            }
            let Some((current, root, file)) = renames::published(&tx, job)? else {
                continue;
            };
            let Some(kind) = checksum_name(file.path.as_str()) else {
                continue;
            };
            if current != publication {
                continue;
            }
            plans.push(ManifestPlan {
                job,
                publication,
                root,
                modified_ns: modified(&tx, publication)?,
                file,
                kind,
            });
            if plans.len() == limit {
                break;
            }
        }
        Ok(plans)
    }

    /// Records what the checksum file `job` published at `publication`
    /// lists, or why it could not be read, then checks its package's
    /// complete downloads against it. Ignored if the file changed since.
    pub(crate) fn record_checksum_file(
        &mut self,
        job: JobId,
        publication: i64,
        read: Result<Manifest, String>,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        let row: Option<(i64, String)> = tx
            .query_row(
                "SELECT package_id, state FROM checksum_files \
                 WHERE job_id = ?1 AND publication_id = ?2",
                params![sql(job)?, publication],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((package, state)) = row else {
            return Ok(());
        };
        if state != "pending" {
            return Ok(());
        }
        let Some((_, _, file)) = renames::published(&tx, job)? else {
            return Ok(());
        };
        let base = folder(file.path.as_str()).to_owned();
        match read {
            Ok(manifest) => {
                let mut insert = tx.prepare(
                    "INSERT INTO checksum_entries (publication_id, seq, path, path_key, \
                     algorithm, value) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                let mut skipped = manifest.skipped;
                for (seq, entry) in manifest.entries.iter().enumerate() {
                    let path = format!("{base}{}", entry.name);
                    if path.len() > 4096 {
                        skipped += 1;
                        continue;
                    }
                    insert.execute(params![
                        publication,
                        i64::try_from(seq).or_corrupt("entry")?,
                        path,
                        path.to_lowercase(),
                        <&str>::from(entry.digest.algorithm()),
                        entry.digest.value(),
                    ])?;
                }
                tx.execute(
                    "UPDATE checksum_files SET state = 'read', skipped = ?3, updated_at = ?4 \
                     WHERE job_id = ?1 AND publication_id = ?2",
                    params![
                        sql(job)?,
                        publication,
                        i64::try_from(skipped).or_corrupt("skipped")?,
                        now.unix_millis()
                    ],
                )?;
            }
            Err(problem) => {
                tx.execute(
                    "UPDATE checksum_files SET state = 'unreadable', problem = ?3, \
                     updated_at = ?4 WHERE job_id = ?1 AND publication_id = ?2",
                    params![
                        sql(job)?,
                        publication,
                        DiagnosticText::new(&problem).as_str(),
                        now.unix_millis()
                    ],
                )?;
            }
        }
        let complete: Vec<i64> = tx
            .prepare(
                "SELECT id FROM jobs WHERE package_id = ?1 AND removed_at IS NULL \
                 AND stage = 'complete' ORDER BY id",
            )?
            .query_map([package], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for id in complete {
            match_job(&tx, key(id)?, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Jobs whose published file has a check waiting, up to `limit`, but
    /// `excluded` ones and those being renamed or replaced.
    pub(crate) fn pending_checks(
        &mut self,
        limit: usize,
        excluded: &[JobId],
    ) -> Result<Vec<JobId>, StoreError> {
        let tx = self.read()?;
        if !checking(&tx)? {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = tx
            .prepare(
                "SELECT DISTINCT v.job_id FROM verifications v \
                 JOIN publications a ON a.id = v.publication_id AND a.state = 'published' \
                 JOIN jobs j ON j.id = v.job_id WHERE v.state = 'pending' \
                 AND j.removed_at IS NULL AND NOT EXISTS ( \
                     SELECT 1 FROM replacements r WHERE r.job_id = v.job_id) \
                 AND (v.source_publication_id = 0 OR EXISTS ( \
                     SELECT 1 FROM publications s WHERE s.id = v.source_publication_id \
                     AND s.state = 'published')) \
                 ORDER BY v.job_id",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut jobs = Vec::new();
        for id in ids {
            let id: JobId = key(id)?;
            if excluded.contains(&id) || renames::pending(&tx, id)? {
                continue;
            }
            jobs.push(id);
            if jobs.len() == limit {
                break;
            }
        }
        Ok(jobs)
    }

    /// What checking `job`'s published file needs now, if it still has a
    /// check waiting. Checks waiting on a checksum file that is no longer
    /// published are dropped first.
    pub(crate) fn check_plan(&mut self, job: JobId) -> Result<Option<CheckPlan>, StoreError> {
        let tx = self.write()?;
        tx.execute(
            "DELETE FROM verifications WHERE job_id = ?1 AND state = 'pending' \
             AND source_publication_id <> 0 AND NOT EXISTS ( \
                 SELECT 1 FROM publications s WHERE s.id = source_publication_id \
                 AND s.state = 'published')",
            [sql(job)?],
        )?;
        let plan = match renames::published(&tx, job)? {
            Some((publication, root, file))
                if !renames::pending(&tx, job)? && !pending(&tx, job)? =>
            {
                let algorithms = tx
                    .prepare(
                        "SELECT DISTINCT algorithm FROM verifications \
                         WHERE publication_id = ?1 AND state = 'pending' ORDER BY algorithm",
                    )?
                    .query_map([publication], |r| r.get::<_, String>(0))?
                    .map(|a| Algorithm::from_str(&a?).or_corrupt("check algorithm"))
                    .collect::<Result<Vec<_>, _>>()?;
                (!algorithms.is_empty()).then(|| -> Result<_, StoreError> {
                    Ok(CheckPlan {
                        publication,
                        root,
                        modified_ns: modified(&tx, publication)?,
                        file,
                        algorithms,
                    })
                })
            }
            _ => None,
        }
        .transpose()?;
        tx.commit()?;
        Ok(plan)
    }

    /// Records how checking `job`'s file at `publication` ended, unless it
    /// was replaced or renamed meanwhile. A file that failed is authorized
    /// to be downloaded again, in the same transaction, if the user turned
    /// that on, no check of it conflicts, and the job has been downloaded
    /// again on its own fewer than [`MAX_CHECK_REDOWNLOADS`] times; the
    /// answer is then the replacement to carry out.
    pub(crate) fn record_file_check(
        &mut self,
        job: JobId,
        publication: i64,
        outcome: CheckOutcome,
        now: UtcTimestamp,
    ) -> Result<Option<Replacement>, StoreError> {
        let tx = self.write()?;
        let current = renames::published(&tx, job)?;
        let Some((_, root, file)) = current.filter(|(p, _, _)| *p == publication) else {
            return Ok(None);
        };
        if renames::pending(&tx, job)? || pending(&tx, job)? {
            return Ok(None);
        }
        let rows: Vec<(i64, String, String, Option<String>)> = tx
            .prepare(
                "SELECT source_publication_id, algorithm, expected, source_name \
                 FROM verifications WHERE publication_id = ?1 AND state = 'pending'",
            )?
            .query_map([publication], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        let mut failed = None;
        for (source, algorithm, expected, source_name) in rows {
            let (state, actual, version, problem) = match &outcome {
                CheckOutcome::Hashed(version, digests) => {
                    let Some(actual) = digests
                        .iter()
                        .find(|d| <&str>::from(d.algorithm()) == algorithm)
                    else {
                        continue;
                    };
                    let matched = actual.value() == expected;
                    if !matched && failed.is_none() {
                        failed = Some((algorithm.clone(), source_name.clone()));
                    }
                    (
                        if matched { "verified" } else { "failed" },
                        Some(actual.value().to_owned()),
                        Some(*version),
                        None,
                    )
                }
                CheckOutcome::Changed(why) => ("changed", None, None, Some(*why)),
            };
            tx.execute(
                "UPDATE verifications SET state = ?4, actual = ?5, file_device = ?6, \
                 file_inode = ?7, file_birth_ns = ?8, file_size = ?9, file_modified_ns = ?10, \
                 problem = ?11, checked_at = ?12 \
                 WHERE publication_id = ?1 AND source_publication_id = ?2 AND algorithm = ?3",
                params![
                    publication,
                    source,
                    algorithm,
                    state,
                    actual,
                    version.map(|v| v.identity.device.cast_signed()),
                    version.map(|v| v.identity.inode.cast_signed()),
                    version.and_then(|v| v.identity.birth_ns),
                    version
                        .map(|v| i64::try_from(v.size).or_corrupt("size"))
                        .transpose()?,
                    version.and_then(|v| v.modified_ns),
                    problem,
                    now.unix_millis()
                ],
            )?;
        }
        jobs::announce(&tx, job, now)?;
        let mut replacement = None;
        if let (Some((algorithm, source)), CheckOutcome::Hashed(version, _)) = (failed, &outcome)
            && super::roots::settings(&tx)?.redownload_failed_checks
            && !has_check(&tx, publication, "conflict")?
        {
            let stored = jobs::load(&tx, job)?;
            let used: i64 = tx.query_row(
                "SELECT check_redownloads FROM jobs WHERE id = ?1",
                [sql(job)?],
                |r| r.get(0),
            )?;
            if !stored.removed
                && used < MAX_CHECK_REDOWNLOADS
                && !deletions::pending(&tx, job)?
                && extractions::check_idle(&tx, job).is_ok()
            {
                tx.execute(
                    "UPDATE jobs SET check_redownloads = check_redownloads + 1 WHERE id = ?1",
                    [sql(job)?],
                )?;
                authorize(
                    &tx,
                    job,
                    publication,
                    stored.state.generation(),
                    version.modified_ns,
                    &reason(&algorithm, source.as_deref()),
                    None,
                    now,
                )?;
                replacement = Some(Replacement {
                    job,
                    root,
                    file,
                    modified_ns: version.modified_ns,
                });
            }
        }
        tx.commit()?;
        Ok(replacement)
    }

    /// Records the provider's checksum that `job`'s file at its current
    /// publication matched before it was published.
    pub(crate) fn record_provider_check(
        &mut self,
        job: JobId,
        generation: Generation,
        digest: &Digest,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        let Some((publication, _, file)) = renames::published(&tx, job)? else {
            return Ok(());
        };
        tx.execute(
            "INSERT OR IGNORE INTO verifications (publication_id, source_publication_id, \
             algorithm, job_id, generation, expected, actual, state, file_size, checked_at) \
             VALUES (?1, 0, ?2, ?3, ?4, ?5, ?5, 'verified', ?6, ?7)",
            params![
                publication,
                <&str>::from(digest.algorithm()),
                sql(job)?,
                sql(generation)?,
                digest.value(),
                i64::try_from(file.size).or_corrupt("size")?,
                now.unix_millis()
            ],
        )?;
        jobs::announce(&tx, job, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Records that `job`'s staged file of `generation` did not match a
    /// checksum its provider gave, in one transaction: the file is given to
    /// cleanup, so neither a retry nor recovery can continue it, and the
    /// job is queued to start from zero if the user has downloading again
    /// on and the job has budget left, else failed. Answers whether it was
    /// queued.
    pub(crate) fn fail_provider_check(
        &mut self,
        job: JobId,
        generation: Generation,
        why: &str,
        now: UtcTimestamp,
    ) -> Result<bool, StoreError> {
        use wayfetch_domain::failure::FailureCode;
        use wayfetch_domain::job::JobStage;
        use wayfetch_protocol::query::FailureSummary;
        let tx = self.write()?;
        let mut stored = super::files::job_at(
            &tx,
            job,
            generation,
            &[JobStage::Verifying],
            "a failed check needs a verifying job",
        )?;
        tx.execute(
            "UPDATE staging_files SET cleanup = 'pending', handoff_generation = NULL \
             WHERE job_id = ?1 AND cleanup IS NULL",
            [sql(job)?],
        )?;
        let used: i64 = tx.query_row(
            "SELECT check_redownloads FROM jobs WHERE id = ?1",
            [sql(job)?],
            |r| r.get(0),
        )?;
        let again = super::roots::settings(&tx)?.redownload_failed_checks
            && used < MAX_CHECK_REDOWNLOADS
            && !stored.removed;
        let previous = stored.state;
        if again {
            tx.execute(
                "UPDATE jobs SET check_redownloads = check_redownloads + 1 WHERE id = ?1",
                [sql(job)?],
            )?;
            stored.state.advance(JobStage::Queued, generation)?;
            jobs::set_restart_reason(
                &tx,
                &mut stored,
                Some(DiagnosticText::new(&format!(
                    "{why}, so it was downloaded again"
                ))),
            )?;
            jobs::save(&tx, &mut stored, &previous, None, now)?;
        } else {
            stored.state.advance(JobStage::Failed, generation)?;
            let failure = FailureSummary {
                code: FailureCode::IntegrityMismatch,
                retry_at: None,
                message: Some(DiagnosticText::new(why)),
            };
            jobs::save(&tx, &mut stored, &previous, Some(failure), now)?;
        }
        tx.commit()?;
        Ok(again)
    }

    /// Whether a retry of `job` downloads its complete file again: it is
    /// complete, and its file failed a check.
    pub(crate) fn failed_check(&mut self, job: JobId) -> Result<bool, StoreError> {
        let tx = self.read()?;
        let stored = jobs::load(&tx, job)?;
        if stored.state.stage() != wayfetch_domain::job::JobStage::Complete {
            return Ok(false);
        }
        match renames::published(&tx, job)? {
            Some((publication, _, _)) => Ok(has_check(&tx, publication, "failed")?
                || has_check(&tx, publication, "changed")?
                || pending(&tx, job)?),
            None => Ok(false),
        }
    }

    /// Records a person's retry of a complete job whose file failed its
    /// check, before the file is touched. Retrying an unfinished one
    /// resumes it.
    pub(crate) fn begin_replacement(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
        now: UtcTimestamp,
    ) -> Result<ReplaceStart, ProtocolError> {
        let Command::Retry {
            job: id,
            expected_revision,
            ..
        } = envelope.command
        else {
            return Err(StoreError::Corrupt("retry command").into());
        };
        let tx = self.write()?;
        let command_key = envelope.idempotency_key.as_uuid().to_string();
        let fingerprint = envelope.command.fingerprint();
        if let Some(held) = holding(&tx, &command_key)? {
            if held != fingerprint.as_bytes() {
                return Err(ProtocolError::new(
                    ErrorCode::IdempotencyConflict,
                    "idempotency key was used for a different command",
                ));
            }
            let plan = replacement(&tx, id)?.or_corrupt("replacement")?;
            return Ok(ReplaceStart::Started(plan));
        }
        if let Some(result) = replayed(&tx, envelope, instance)? {
            return Ok(ReplaceStart::Done(result));
        }
        let job = jobs::load_listed(&tx, id)?;
        job.state
            .check_complete(expected_revision)
            .map_err(StoreError::from)?;
        check_idle(&tx, id)?;
        if renames::pending(&tx, id)? {
            return Err(StoreError::Busy("the file is being renamed; try again").into());
        }
        deletions::check_idle(&tx, id)?;
        extractions::check_idle(&tx, id)?;
        let (publication, _, _) = renames::published(&tx, id)?.or_corrupt("publication")?;
        if has_check(&tx, publication, "changed")? {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "the file was changed outside Wayfetch, so it is not removed; delete or \
                 remove it yourself to download it again",
            ));
        }
        let failed: Option<(String, Option<String>, Option<i64>)> = tx
            .query_row(
                "SELECT algorithm, source_name, file_modified_ns FROM verifications \
                 WHERE publication_id = ?1 AND state = 'failed' \
                 ORDER BY source_publication_id LIMIT 1",
                [publication],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        let Some((algorithm, source, seen)) = failed else {
            return Err(ProtocolError::new(
                ErrorCode::NotApplicable,
                "the download is complete and its file passed its checks",
            ));
        };
        authorize(
            &tx,
            id,
            publication,
            job.state.generation(),
            seen,
            &reason(&algorithm, source.as_deref()),
            Some((&command_key, fingerprint.as_bytes())),
            now,
        )?;
        let plan = replacement(&tx, id)?.or_corrupt("replacement")?;
        tx.commit().map_err(StoreError::from)?;
        Ok(ReplaceStart::Started(plan))
    }

    /// Jobs with an unfinished replacement.
    pub(crate) fn pending_replacements(&mut self) -> Result<Vec<JobId>, StoreError> {
        let tx = self.read()?;
        tx.prepare("SELECT job_id FROM replacements ORDER BY job_id")?
            .query_map([], |r| r.get(0))?
            .map(|id| key(id?))
            .collect()
    }

    /// `job`'s unfinished replacement, if it has one. One whose file is
    /// no longer published is dropped.
    pub(crate) fn replacement(&mut self, job: JobId) -> Result<Option<Replacement>, StoreError> {
        let tx = self.write()?;
        let plan = replacement(&tx, job)?;
        if plan.is_none() {
            tx.execute("DELETE FROM replacements WHERE job_id = ?1", [sql(job)?])?;
        }
        tx.commit()?;
        Ok(plan)
    }

    /// Finishes `job`'s replacement once its file is gone: drops the
    /// publication and its checks, resets unpacking, and queues the job
    /// under a new generation, recording a person's command result. Answers
    /// that result, if a person asked.
    pub(crate) fn finish_replacement(
        &mut self,
        job: JobId,
        now: UtcTimestamp,
    ) -> Result<Option<CommandResult>, StoreError> {
        let tx = self.write()?;
        let row: Option<ReplacementRow> = tx
            .query_row(
                "SELECT publication_id, generation, reason, idempotency_key, fingerprint \
                 FROM replacements WHERE job_id = ?1",
                [sql(job)?],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((publication, generation, why, command_key, fingerprint)) = row else {
            return Ok(None);
        };
        let mut stored = jobs::load(&tx, job)?;
        if sql(stored.state.generation())? != generation {
            return Err(StoreError::Corrupt("replacement generation"));
        }
        tx.execute("DELETE FROM replacements WHERE job_id = ?1", [sql(job)?])?;
        tx.execute("DELETE FROM publications WHERE id = ?1", [publication])?;
        forget_publication(&tx, publication)?;
        tx.execute(
            "DELETE FROM extractions WHERE job_id = ?1 AND staging IS NULL",
            [sql(job)?],
        )?;
        tx.execute(
            "UPDATE extractions SET state = 'cancelled', output = NULL, problem = NULL \
             WHERE job_id = ?1",
            [sql(job)?],
        )?;
        let previous = stored.state;
        stored.state.replace()?;
        jobs::set_restart_reason(&tx, &mut stored, Some(DiagnosticText::new(&why)))?;
        jobs::save(&tx, &mut stored, &previous, None, now)?;
        let result = CommandResult::JobUpdated {
            job,
            revision: stored.state.revision(),
            changed: true,
        };
        let answered = match (command_key, fingerprint) {
            (Some(command_key), Some(fingerprint)) => {
                record_as(&tx, &command_key, &fingerprint, &result, now)?;
                Some(result)
            }
            _ => None,
        };
        tx.commit()?;
        Ok(answered)
    }

    /// Drops `job`'s replacement without removing its file, because the
    /// file was changed or replaced outside Wayfetch: its checks say so,
    /// and nothing is downloaded until the person acts.
    pub(crate) fn refuse_replacement(
        &mut self,
        job: JobId,
        why: &str,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        let publication: Option<i64> = tx
            .query_row(
                "SELECT publication_id FROM replacements WHERE job_id = ?1",
                [sql(job)?],
                |r| r.get(0),
            )
            .optional()?;
        let Some(publication) = publication else {
            return Ok(());
        };
        tx.execute("DELETE FROM replacements WHERE job_id = ?1", [sql(job)?])?;
        tx.execute(
            "UPDATE verifications SET state = 'changed', problem = ?2 \
             WHERE publication_id = ?1 AND state = 'failed'",
            params![publication, why],
        )?;
        jobs::announce(&tx, job, now)?;
        tx.commit()?;
        Ok(())
    }
}

/// Why a job is downloaded again, as shown to the person.
fn reason(algorithm: &str, source: Option<&str>) -> String {
    let algorithm = Algorithm::from_str(algorithm).map_or("its", |a| match a {
        Algorithm::Crc32 => "the CRC32",
        Algorithm::Md5 => "the MD5",
        Algorithm::Sha1 => "the SHA-1",
        Algorithm::Sha256 => "the SHA-256",
    });
    match source {
        Some(source) => format!(
            "the downloaded file did not match {algorithm} checksum in {source}, so it was \
             downloaded again"
        ),
        None => format!(
            "the downloaded file did not match {algorithm} checksum the provider gave, so it \
             was downloaded again"
        ),
    }
}

/// Records the authority to replace `job`'s file at `publication`, seen
/// modified at `modified_ns` by the failing check.
#[allow(clippy::too_many_arguments)]
fn authorize(
    tx: &Transaction<'_>,
    job: JobId,
    publication: i64,
    generation: Generation,
    modified_ns: Option<i64>,
    why: &str,
    command: Option<(&str, &[u8])>,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO replacements (job_id, publication_id, generation, file_modified_ns, \
         reason, idempotency_key, fingerprint, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            sql(job)?,
            publication,
            sql(generation)?,
            modified_ns,
            DiagnosticText::new(why).as_str(),
            command.map(|(k, _)| k),
            command.map(|(_, f)| f),
            now.unix_millis()
        ],
    )?;
    jobs::announce(tx, job, now)
}

/// `job`'s recorded replacement, if its file is still the publication it
/// names.
fn replacement(tx: &Transaction<'_>, job: JobId) -> Result<Option<Replacement>, StoreError> {
    let row: Option<(i64, Option<i64>)> = tx
        .query_row(
            "SELECT publication_id, file_modified_ns FROM replacements WHERE job_id = ?1",
            [sql(job)?],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((publication, modified_ns)) = row else {
        return Ok(None);
    };
    Ok(renames::published(tx, job)?
        .filter(|(current, _, _)| *current == publication)
        .map(|(_, root, file)| Replacement {
            job,
            root,
            file,
            modified_ns,
        }))
}
