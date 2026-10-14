//! No-clobber publication and its crash reconciliation (section 7.7, tests
//! T24 and T31).

use std::io;

use cap_std::fs::Dir;

use super::{FileIdentity, FsError, RelativePath, Root, StagingFile, StagingRecord, sys};

/// Where an interrupted publication stands, judged by file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationCheck {
    /// The staging file is intact and the target name is free: publish
    /// again.
    NotPublished,
    /// The target is the staged file: complete the publication.
    Published,
    /// The staging file is intact but another file holds the target name.
    /// Nothing was touched; reserve another name.
    Conflict,
    /// Neither name holds the staged file. The partial must restart.
    Lost,
}

impl Root {
    /// Publishes a finished staging file at `target`, creating missing
    /// parent directories. The caller has recorded a publication intent
    /// first.
    ///
    /// The file is synchronized, hard-linked to the target name, which
    /// fails rather than replacing anything there, the target directory is
    /// synchronized, and then the staging name is removed. Any crash leaves
    /// a state [`Root::reconcile_publication`] can classify.
    pub fn publish(&self, staging: StagingFile, target: &RelativePath) -> Result<(), FsError> {
        self.verify()?;
        staging
            .sync_data()
            .map_err(FsError::io("sync staging file"))?;
        let record = staging.record();
        let (parent, name) = target.split_last();
        let parent = self
            .parent_dir(parent, true)?
            .expect("created parents exist");
        match self.staging.hard_link(&record.name, &parent, name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::TargetExists(target.clone()));
            }
            Err(e) => return Err(FsError::io("link publication target")(e)),
        }
        // The staging name could have been swapped after it was opened; then
        // the name just created is not ours to keep.
        if !matches(identity_at(&parent, name)?, &record.identity) {
            parent
                .remove_file(name)
                .map_err(FsError::io("remove mislinked target"))?;
            return Err(FsError::StagingReplaced(record.name.clone()));
        }
        sys::sync_dir(&parent).map_err(FsError::io("sync target directory"))?;
        self.remove_staging_name(record)
    }

    /// Classifies an interrupted publication of `record` at `target` after
    /// a restart. A staging name left behind by an otherwise complete
    /// publication is removed. Nothing else is changed, and a file that is
    /// not the staged one is never deleted or overwritten.
    pub fn reconcile_publication(
        &self,
        record: &StagingRecord,
        target: &RelativePath,
    ) -> Result<PublicationCheck, FsError> {
        // An unmounted root would make every file look lost.
        self.verify()?;
        let (parent, name) = target.split_last();
        let at_target = match self.parent_dir(parent, false)? {
            Some(dir) => identity_at(&dir, name)?,
            None => None,
        };
        let staged = matches(identity_at(&self.staging, &record.name)?, &record.identity);
        Ok(if matches(at_target, &record.identity) {
            if staged {
                self.remove_staging_name(record)?;
            }
            PublicationCheck::Published
        } else if staged && at_target.is_some() {
            PublicationCheck::Conflict
        } else if staged {
            PublicationCheck::NotPublished
        } else {
            PublicationCheck::Lost
        })
    }

    fn remove_staging_name(&self, record: &StagingRecord) -> Result<(), FsError> {
        self.staging
            .remove_file(&record.name)
            .map_err(FsError::io("remove staging name"))?;
        sys::sync_dir(&self.staging).map_err(FsError::io("sync staging directory"))
    }

    /// Opens the directory `parent` below the root, or the root itself.
    /// With `create`, missing directories are created and their entries
    /// synchronized; without it, a missing directory is `None`.
    fn parent_dir(&self, parent: Option<&str>, create: bool) -> Result<Option<Dir>, FsError> {
        let mut dir = self.dir.try_clone().map_err(FsError::io("open root"))?;
        for component in parent.into_iter().flat_map(|p| p.split('/')) {
            dir = match dir.open_dir(component) {
                Ok(next) => next,
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    match dir.create_dir(component) {
                        Ok(()) => sys::sync_dir(&dir).map_err(FsError::io("sync directory"))?,
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(FsError::io("create directory")(e)),
                    }
                    dir.open_dir(component)
                        .map_err(FsError::io("open directory"))?
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(FsError::io("open directory")(e)),
            };
        }
        Ok(Some(dir))
    }
}

fn matches(found: Option<FileIdentity>, expected: &FileIdentity) -> bool {
    found.is_some_and(|found| found.matches(expected))
}

/// Identity of whatever has `name` in `dir`, without following a symlink.
fn identity_at(dir: &Dir, name: &str) -> Result<Option<FileIdentity>, FsError> {
    match dir.symlink_metadata(name) {
        Ok(meta) => sys::identity(&meta).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(FsError::io("inspect file")(e)),
    }
}

#[cfg(test)]
mod tests {
    use wayfetch_domain::version::Generation;

    use super::*;
    use crate::fs::staging::tests::{job, root, staging_path};

    fn path(p: &str) -> RelativePath {
        RelativePath::new(p).unwrap()
    }

    fn staged(root: &Root, generation: u64, bytes: &[u8]) -> StagingFile {
        let staging = root
            .create_staging(job(), Generation::from_u64(generation).unwrap())
            .unwrap();
        staging.write_all_at(bytes, 0).unwrap();
        staging
    }

    #[test]
    fn publishes_without_clobbering() {
        let (tmp, root) = root();
        let staging = staged(&root, 1, b"artifact");
        let record = staging.record().clone();
        root.publish(staging, &path("Movies/new/a.bin")).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("Movies/new/a.bin")).unwrap(),
            b"artifact"
        );
        assert!(!staging_path(tmp.path(), &record.name).exists());
        assert_eq!(
            root.reconcile_publication(&record, &path("Movies/new/a.bin"))
                .unwrap(),
            PublicationCheck::Published
        );

        // An unrelated file already at the target survives untouched.
        std::fs::write(tmp.path().join("b.bin"), b"user data").unwrap();
        let staging = staged(&root, 2, b"other");
        let record = staging.record().clone();
        assert!(matches!(
            root.publish(staging, &path("b.bin")),
            Err(FsError::TargetExists(_))
        ));
        assert_eq!(
            std::fs::read(tmp.path().join("b.bin")).unwrap(),
            b"user data"
        );
        assert_eq!(
            root.reconcile_publication(&record, &path("b.bin")).unwrap(),
            PublicationCheck::Conflict
        );
        // The staged file is intact for another name.
        let staging = root.reopen_staging(&record, 5).unwrap();
        root.publish(staging, &path("b (1).bin")).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("b (1).bin")).unwrap(),
            b"other"
        );
    }

    #[test]
    fn reconciles_each_crash_point() {
        let (tmp, root) = root();
        let target = path("dir/a.bin");

        // Crash after the intent, before the link.
        let staging = staged(&root, 1, b"x");
        let record = staging.record().clone();
        drop(staging);
        assert_eq!(
            root.reconcile_publication(&record, &target).unwrap(),
            PublicationCheck::NotPublished
        );

        // Crash after the link, before the staging name was removed.
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::hard_link(
            staging_path(tmp.path(), &record.name),
            tmp.path().join("dir/a.bin"),
        )
        .unwrap();
        assert_eq!(
            root.reconcile_publication(&record, &target).unwrap(),
            PublicationCheck::Published
        );
        assert!(!staging_path(tmp.path(), &record.name).exists());
        assert_eq!(std::fs::read(tmp.path().join("dir/a.bin")).unwrap(), b"x");

        // Both names gone: nothing to publish.
        let staging = staged(&root, 2, b"y");
        let record = staging.record().clone();
        drop(staging);
        std::fs::remove_file(staging_path(tmp.path(), &record.name)).unwrap();
        assert_eq!(
            root.reconcile_publication(&record, &path("other.bin"))
                .unwrap(),
            PublicationCheck::Lost
        );
    }

    #[test]
    fn reconcile_refuses_an_unmounted_root() {
        let (tmp, root) = root();
        let staging = staged(&root, 1, b"x");
        let record = staging.record().clone();
        drop(staging);
        std::fs::remove_dir_all(tmp.path().join(crate::fs::PRIVATE_DIR)).unwrap();
        assert!(matches!(
            root.reconcile_publication(&record, &path("a.bin")),
            Err(FsError::SentinelMissing(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn targets_cannot_escape_through_symlinks() {
        let (tmp, root) = root();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();
        let staging = staged(&root, 1, b"x");
        assert!(root.publish(staging, &path("escape/a.bin")).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
