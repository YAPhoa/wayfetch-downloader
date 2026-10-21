//! Generation-private staging files (section 7.6, tests T27 and T28).

use std::fs::File;
use std::io;
use std::ops::Range;

use cap_std::fs::OpenOptions;
use wayfetch_domain::id::{JobId, RootId};
use wayfetch_domain::version::Generation;

use super::{FileIdentity, FsError, Root, sys};

/// What the store records about a staging file: the root and name to find
/// it by, the identity it must still have, and the job generation that owns
/// it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingRecord {
    /// Root holding the file.
    pub root: RootId,
    /// Owning job.
    pub job: JobId,
    /// Owning generation. A file adopted after a restart keeps its name but
    /// changes owner.
    pub generation: Generation,
    /// Name inside the root's staging directory: `<instance>/<file>` in
    /// the folder of the profile that created it, or a bare file name for
    /// one an earlier version staged directly in the directory.
    pub name: String,
    /// Identity of the file created under that name.
    pub identity: FileIdentity,
}

/// An open staging file owned by one job generation.
#[derive(Debug)]
pub struct StagingFile {
    record: StagingRecord,
    file: File,
}

impl StagingFile {
    /// Who owns the file and how to find it again.
    pub fn record(&self) -> &StagingRecord {
        &self.record
    }

    /// Current file length. Not coverage: a preallocated or sparse file is
    /// longer than its written data (T25).
    pub fn file_len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Writes `buf` at `offset`. Blocking.
    pub fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        sys::write_all_at(&self.file, buf, offset)
    }

    /// Cuts the file to `len` bytes. Only for publication, while the file is
    /// still private. Blocking.
    pub(super) fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    /// Synchronizes the file's data to stable storage. Blocking.
    pub fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Synchronizes the file, then certifies `ranges` of it for the job
    /// generation that owns it, as the only way to build a checkpoint. The
    /// caller must have finished writing every byte of `ranges`. A sync
    /// error is a failure, never progress. Blocking.
    pub fn sync_ranges(&self, ranges: Vec<Range<u64>>) -> io::Result<SyncedRanges> {
        self.sync_data()?;
        Ok(SyncedRanges {
            file: self.record.clone(),
            ranges,
        })
    }
}

/// Byte ranges of a staging file certified durable on stable storage,
/// waiting for the store to commit them as a checkpoint.
#[derive(Debug)]
pub struct SyncedRanges {
    pub(crate) file: StagingRecord,
    pub(crate) ranges: Vec<Range<u64>>,
}

impl SyncedRanges {
    /// The file the ranges belong to.
    pub fn file(&self) -> &StagingRecord {
        &self.file
    }

    /// The certified ranges.
    pub fn ranges(&self) -> &[Range<u64>] {
        &self.ranges
    }
}

impl Root {
    /// Creates the private staging file for `job` at `generation` in this
    /// profile's staging folder. The name is derived from both and created
    /// exclusively, so no other generation can share its inode, and the
    /// folder is the profile's own, so another profile staging the same job
    /// and generation IDs in the same root never meets it. The directory entry is synchronized before this
    /// returns, so a store record made afterwards never names a file a crash
    /// could lose. A crash before the record commits leaves an unowned file,
    /// which is never adopted and which the next start removes.
    pub fn create_staging(
        &self,
        job: JobId,
        generation: Generation,
    ) -> Result<StagingFile, FsError> {
        self.create_named(job, generation, staging_name(job, generation))
    }

    /// Creates the private file a media conversion of `job` at
    /// `generation` writes, beside its staging file, exclusively and with
    /// its directory entry synchronized, like [`Root::create_staging`].
    pub fn create_media_output(
        &self,
        job: JobId,
        generation: Generation,
    ) -> Result<StagingFile, FsError> {
        self.create_named(job, generation, format!("{job}.g{generation}.media"))
    }

    /// Where the staging file `name` is, for a helper program that opens
    /// it by path. The path is only as trustworthy as the root: the file's
    /// identity is checked again before its contents are used.
    pub fn staging_file_path(&self, name: &str) -> std::path::PathBuf {
        self.path()
            .join(super::PRIVATE_DIR)
            .join(super::root::STAGING)
            .join(name)
    }

    /// The directory holding the staging file `record` names, and its name
    /// there: this profile's folder, or the shared directory for a file an
    /// earlier version staged. A record of another root, or a name in
    /// another profile's folder, finds nothing: that file is never touched.
    pub(super) fn locate<'a>(
        &'a self,
        record: &'a StagingRecord,
    ) -> Result<(&'a cap_std::fs::Dir, &'a str), FsError> {
        if record.root != self.id {
            return Err(FsError::WrongRoot(record.root));
        }
        match record.name.split_once('/') {
            None => Ok((&self.legacy_staging, &record.name)),
            Some((owner, file)) if owner == self.owner && !file.contains('/') => {
                Ok((&self.staging, file))
            }
            Some(_) => Err(FsError::StagingMissing(record.name.clone())),
        }
    }

    fn create_named(
        &self,
        job: JobId,
        generation: Generation,
        name: String,
    ) -> Result<StagingFile, FsError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let file = self
            .staging
            .open_with(&name, &options)
            .map_err(FsError::io("create staging file"))?;
        sys::sync_dir(&self.staging).map_err(FsError::io("sync staging directory"))?;
        let name = format!("{}/{name}", self.owner);
        let identity = sys::identity(
            &file
                .metadata()
                .map_err(FsError::io("inspect staging file"))?,
        )?;
        Ok(StagingFile {
            record: StagingRecord {
                root: self.id,
                job,
                generation,
                name,
                identity,
            },
            file: file.into_std(),
        })
    }

    /// Reopens a recorded staging file whose durable coverage ends at
    /// `durable_end`, after checking that it is still the file that was
    /// created and still holds every durable byte. A missing, replaced, or
    /// shrunk file is a reconciliation failure: its partial must restart,
    /// never be appended to (T28).
    ///
    /// Content changed in place by something else cannot be detected here;
    /// verification of the finished file catches it.
    pub fn reopen_staging(
        &self,
        record: &StagingRecord,
        durable_end: u64,
    ) -> Result<StagingFile, FsError> {
        let (dir, name) = self.locate(record)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let file = match dir.open_with(name, &options) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(FsError::StagingMissing(record.name.clone()));
            }
            Err(e) => return Err(FsError::io("open staging file")(e)),
        };
        let meta = file
            .metadata()
            .map_err(FsError::io("inspect staging file"))?;
        if !sys::identity(&meta)?.matches(&record.identity) || !meta.is_file() {
            return Err(FsError::StagingReplaced(record.name.clone()));
        }
        if meta.len() < durable_end {
            return Err(FsError::StagingShrunk {
                name: record.name.clone(),
                len: meta.len(),
                needed: durable_end,
            });
        }
        Ok(StagingFile {
            record: record.clone(),
            file: file.into_std(),
        })
    }
}

/// The name of the staging file of `job` at `generation`.
fn staging_name(job: JobId, generation: Generation) -> String {
    format!("{job}.g{generation}.part")
}

/// What removing a staging file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// The file was removed, and the removal is durable.
    Removed,
    /// Nothing has the name any more, durably.
    Missing,
    /// The name holds something other than the expected file, which was
    /// left in place.
    Foreign,
}

impl Root {
    /// Removes the recorded staging file `record`, after checking that its
    /// name still holds the very file that was created: a regular file
    /// with the recorded identity. Anything else is left in place. Whether
    /// this removed the name or found it already gone, perhaps removed by
    /// a pass a crash interrupted before its directory sync, the directory
    /// is synchronized before this returns, so a record deleted afterwards
    /// never outlives a name a crash could bring back.
    ///
    /// Only a recorded identity proves the engine created a file, so the
    /// staging directory is never scanned. Other profiles' files, and a
    /// file that no record names (a crash between creating and registering
    /// it), are never removed: a name alone does not prove the file is
    /// this engine's.
    ///
    /// The check and the removal are two steps; a file swapped in between
    /// by something else with write access to the private staging
    /// directory would be removed.
    pub fn remove_staging(&self, record: &StagingRecord) -> Result<Removal, FsError> {
        let (dir, name) = match self.locate(record) {
            Ok(found) => found,
            Err(FsError::StagingMissing(_)) => return Ok(Removal::Foreign),
            Err(e) => return Err(e),
        };
        // Never follows a symlink: a link planted under the name is foreign.
        let found = match dir.symlink_metadata(name) {
            Ok(meta) if !meta.is_file() || !sys::identity(&meta)?.matches(&record.identity) => {
                return Ok(Removal::Foreign);
            }
            Ok(_) => {
                crash_point!("cleanup-before-unlink");
                match dir.remove_file(name) {
                    Ok(()) => Removal::Removed,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Removal::Missing,
                    Err(e) => return Err(FsError::io("remove staging file")(e)),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Removal::Missing,
            Err(e) => return Err(FsError::io("inspect staging file")(e)),
        };
        sys::sync_dir(dir).map_err(FsError::io("sync staging directory"))?;
        Ok(found)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use wayfetch_protocol::InstanceId;

    use super::*;
    use crate::fs::PRIVATE_DIR;

    /// An initialized, open root in a fresh temporary directory.
    pub(crate) fn root() -> (tempfile::TempDir, Root) {
        let tmp = tempfile::tempdir().unwrap();
        let id = RootId::from_u64(1).unwrap();
        let instance = InstanceId::generate();
        Root::initialize(tmp.path(), id, instance).unwrap();
        let root = Root::open(tmp.path(), id, instance).unwrap();
        (tmp, root)
    }

    pub(crate) fn job() -> JobId {
        JobId::from_u64(7).unwrap()
    }

    pub(crate) fn staging_path(tmp: &Path, name: &str) -> std::path::PathBuf {
        tmp.join(PRIVATE_DIR).join("staging").join(name)
    }

    /// The recorded name of `file` in `root`'s own staging folder.
    pub(crate) fn own_name(root: &Root, file: &str) -> String {
        format!("{}/{file}", root.owner)
    }

    #[test]
    fn each_generation_gets_its_own_file() {
        let (tmp, root) = root();
        let first = root.create_staging(job(), Generation::INITIAL).unwrap();
        let second = root
            .create_staging(job(), Generation::INITIAL.next().unwrap())
            .unwrap();
        assert_ne!(first.record().identity, second.record().identity);
        assert!(staging_path(tmp.path(), &first.record().name).is_file());
        // The same generation can never be created twice.
        assert!(root.create_staging(job(), Generation::INITIAL).is_err());
    }

    #[test]
    fn reopen_accepts_only_the_owned_intact_file() {
        let (tmp, root) = root();
        let staging = root.create_staging(job(), Generation::INITIAL).unwrap();
        staging.write_all_at(b"0123456789", 0).unwrap();
        staging.sync_data().unwrap();
        let record = staging.record().clone();
        drop(staging);
        let path = staging_path(tmp.path(), &record.name);

        let reopened = root.reopen_staging(&record, 10).unwrap();
        assert_eq!(reopened.file_len().unwrap(), 10);
        drop(reopened);

        // Shorter than the durable coverage.
        assert!(matches!(
            root.reopen_staging(&record, 11),
            Err(FsError::StagingShrunk {
                len: 10,
                needed: 11,
                ..
            })
        ));

        // Atomically replaced by another file with the same length.
        let swap = path.with_extension("swap");
        std::fs::write(&swap, b"0123456789").unwrap();
        std::fs::rename(&swap, &path).unwrap();
        assert!(matches!(
            root.reopen_staging(&record, 10),
            Err(FsError::StagingReplaced(_))
        ));

        // Deleted and recreated, which may reuse the inode number; the
        // birth time tells them apart where the filesystem reports it.
        if record.identity.birth_ns.is_some() {
            std::fs::remove_file(&path).unwrap();
            std::fs::write(&path, b"0123456789").unwrap();
            assert!(matches!(
                root.reopen_staging(&record, 10),
                Err(FsError::StagingReplaced(_))
            ));
        }

        std::fs::remove_file(&path).unwrap();
        let err = root.reopen_staging(&record, 10).unwrap_err();
        assert!(matches!(err, FsError::StagingMissing(_)));
        assert_eq!(
            err.failure_code(),
            wayfetch_domain::failure::FailureCode::IntegrityMismatch
        );
    }

    #[cfg(unix)]
    #[test]
    fn reopen_does_not_follow_a_planted_symlink() {
        let (tmp, root) = root();
        let staging = root.create_staging(job(), Generation::INITIAL).unwrap();
        let record = staging.record().clone();
        drop(staging);
        let path = staging_path(tmp.path(), &record.name);
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        assert!(root.reopen_staging(&record, 0).is_err());
    }

    #[test]
    fn removal_takes_only_the_recorded_file() {
        let (tmp, root) = root();
        let staging = root.create_staging(job(), Generation::INITIAL).unwrap();
        let record = staging.record().clone();
        drop(staging);
        let path = staging_path(tmp.path(), &record.name);

        // Replaced by another file under the same name.
        std::fs::write(path.with_extension("swap"), b"theirs").unwrap();
        std::fs::rename(path.with_extension("swap"), &path).unwrap();
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Foreign);
        assert_eq!(std::fs::read(&path).unwrap(), b"theirs");

        // A directory under the name.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Foreign);
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Missing);

        let staging = root
            .create_staging(job(), Generation::INITIAL.next().unwrap())
            .unwrap();
        let record = staging.record().clone();
        drop(staging);
        let _ = crate::fs::sys::tests::take_synced();
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Removed);
        assert!(!staging_path(tmp.path(), &record.name).exists());
        // The removal was made durable.
        assert_eq!(crate::fs::sys::tests::take_synced().len(), 1);
        // Already gone, perhaps unlinked by a pass a crash stopped before
        // its sync: the directory is synced before that is accepted.
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Missing);
        assert_eq!(crate::fs::sys::tests::take_synced().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn removal_never_follows_a_link() {
        let (tmp, root) = root();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();
        let staging = root.create_staging(job(), Generation::INITIAL).unwrap();
        let record = staging.record().clone();
        drop(staging);
        let path = staging_path(tmp.path(), &record.name);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Foreign);
        assert!(path.is_symlink());
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"outside");
    }

    /// Two profiles' roots in one shared folder, each with its own ID.
    pub(crate) fn shared() -> (tempfile::TempDir, Root, Root) {
        let tmp = tempfile::tempdir().unwrap();
        let open = |id| {
            let (id, instance) = (RootId::from_u64(id).unwrap(), InstanceId::generate());
            Root::initialize(tmp.path(), id, instance).unwrap();
            Root::open(tmp.path(), id, instance).unwrap()
        };
        let (a, b) = (open(1), open(1));
        (tmp, a, b)
    }

    #[test]
    fn profiles_sharing_a_root_stage_apart() {
        let (tmp, a, b) = shared();
        // The same job and generation IDs, in two profiles' stores.
        let ours = a.create_staging(job(), Generation::INITIAL).unwrap();
        let theirs = b.create_staging(job(), Generation::INITIAL).unwrap();
        let (ours, theirs) = (ours.record().clone(), theirs.record().clone());
        assert_ne!(ours.name, theirs.name);
        assert_ne!(ours.identity, theirs.identity);
        let path = |r: &StagingRecord| staging_path(tmp.path(), &r.name);
        assert_eq!(
            path(&ours),
            tmp.path()
                .join(PRIVATE_DIR)
                .join("staging")
                .join(a.owner.as_str())
                .join(format!("{}.g{}.part", job(), Generation::INITIAL))
        );
        assert!(path(&ours).is_file() && path(&theirs).is_file());

        // Neither can reach the other's file, even by its record.
        let foreign = StagingRecord {
            root: a.id(),
            ..theirs.clone()
        };
        assert!(matches!(
            a.reopen_staging(&foreign, 0),
            Err(FsError::StagingMissing(_))
        ));
        assert_eq!(a.remove_staging(&foreign).unwrap(), Removal::Foreign);
        assert!(path(&theirs).is_file());
        // Removing its own leaves the other's in place.
        assert_eq!(a.remove_staging(&ours).unwrap(), Removal::Removed);
        assert!(!path(&ours).exists());
        b.reopen_staging(&theirs, 0).unwrap();
    }

    #[test]
    fn a_partial_an_earlier_version_staged_is_still_continued() {
        let (tmp, root) = root();
        // An earlier version staged directly in the shared directory.
        let name = format!("{}.g{}.part", job(), Generation::INITIAL);
        let path = staging_path(tmp.path(), &name);
        std::fs::write(&path, b"0123456789").unwrap();
        let identity = sys::identity(&root.legacy_staging.metadata(&name).unwrap()).unwrap();
        let record = StagingRecord {
            root: root.id(),
            job: job(),
            generation: Generation::INITIAL,
            name,
            identity,
        };
        let file = root.reopen_staging(&record, 10).unwrap();
        file.write_all_at(b"abc", 10).unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789abc");
        assert_eq!(root.remove_staging(&record).unwrap(), Removal::Removed);
        assert!(!path.exists());
        // New files go in the profile's own folder.
        let next = root.create_staging(job(), Generation::INITIAL).unwrap();
        assert_eq!(
            next.record().name,
            own_name(&root, &format!("{}.g{}.part", job(), Generation::INITIAL))
        );
    }
}
