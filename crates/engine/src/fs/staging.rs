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
    /// Name inside the root's staging directory.
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
    /// Creates the private staging file for `job` at `generation`. The name
    /// is derived from both and created exclusively, so no other generation
    /// can share its inode. The directory entry is synchronized before this
    /// returns, so a store record made afterwards never names a file a crash
    /// could lose. A crash before the record commits leaves an unowned file
    /// for a later cleanup; it is never adopted.
    pub fn create_staging(
        &self,
        job: JobId,
        generation: Generation,
    ) -> Result<StagingFile, FsError> {
        let name = format!("{job}.g{generation}.part");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        let file = self
            .staging
            .open_with(&name, &options)
            .map_err(FsError::io("create staging file"))?;
        sys::sync_dir(&self.staging).map_err(FsError::io("sync staging directory"))?;
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
        if record.root != self.id {
            return Err(FsError::WrongRoot(record.root));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        let file = match self.staging.open_with(&record.name, &options) {
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
}
