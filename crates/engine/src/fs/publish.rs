//! No-clobber publication, renaming published files, and their crash
//! reconciliation (section 7.7, tests T24 and T31).

use std::io;
use std::path::PathBuf;

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
    /// The staged file holds a name but no longer has the intended size,
    /// for example because something truncated or extended it. It must not
    /// complete; the partial must restart.
    Damaged,
}

/// A published file as the store records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedFile {
    /// Where it is in the root.
    pub path: RelativePath,
    /// The file published there.
    pub identity: FileIdentity,
    /// Its size.
    pub size: u64,
}

/// Where an interrupted rename stands, judged by file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameCheck {
    /// The new name does not hold the file: the rename did not happen.
    NotRenamed,
    /// The new name holds the file, and the old name no longer does.
    Renamed,
}

impl Root {
    /// Publishes a finished staging file at `target` as an artifact of
    /// `size` bytes, creating missing parent directories. The caller has
    /// recorded a publication intent for that size first.
    ///
    /// Bytes past `size` (uncertified data or preallocation) are cut off
    /// while the file is still private; a file shorter than `size` is
    /// refused. The file is synchronized, hard-linked to the target name,
    /// which fails rather than replacing anything there, checked to be the
    /// staged file at `size` bytes, the target directory is synchronized,
    /// and then the staging name is removed. Any crash leaves a state
    /// [`Root::reconcile_publication`] can classify.
    pub fn publish(
        &self,
        staging: StagingFile,
        target: &RelativePath,
        size: u64,
    ) -> Result<(), FsError> {
        self.verify()?;
        let record = staging.record();
        let len = staging
            .file_len()
            .map_err(FsError::io("inspect staging file"))?;
        if len < size {
            return Err(FsError::StagingShrunk {
                name: record.name.clone(),
                len,
                needed: size,
            });
        }
        if len > size {
            staging
                .set_len(size)
                .map_err(FsError::io("trim staging file"))?;
        }
        staging
            .sync_data()
            .map_err(FsError::io("sync staging file"))?;
        let (parent, name) = target.split_last();
        let parent = self
            .parent_dir(parent, true)?
            .expect("created parents exist");
        crash_point!("publish-before-link");
        match self.staging.hard_link(&record.name, &parent, name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::TargetExists(target.clone()));
            }
            Err(e) => return Err(FsError::io("link publication target")(e)),
        }
        // The staging name could have been swapped or changed after it was
        // opened; then the name just created is not ours to keep.
        if !is_staged(file_at(&parent, name)?, &record.identity, size) {
            parent
                .remove_file(name)
                .map_err(FsError::io("remove mislinked target"))?;
            return Err(FsError::StagingReplaced(record.name.clone()));
        }
        sys::sync_dir(&parent).map_err(FsError::io("sync target directory"))?;
        crash_point!("publish-before-unlink");
        self.remove_staging_name(record)?;
        crash_point!("publish-before-complete");
        Ok(())
    }

    /// Classifies an interrupted publication of `record` at `target` as an
    /// artifact of `size` bytes after a restart. It is `Published` only if
    /// the target is the staged file at exactly `size` bytes; the target
    /// directory is then synchronized again, since the crash may have come
    /// before that, and only after that succeeds is a leftover staging name
    /// removed. Nothing else is changed, and a file that is not the staged
    /// one is never deleted or overwritten.
    pub fn reconcile_publication(
        &self,
        record: &StagingRecord,
        target: &RelativePath,
        size: u64,
    ) -> Result<PublicationCheck, FsError> {
        // An unmounted root would make every file look lost.
        self.verify()?;
        let (parent, name) = target.split_last();
        let parent = self.parent_dir(parent, false)?;
        let at_target = match &parent {
            Some(dir) => file_at(dir, name)?,
            None => None,
        };
        let staged = file_at(&self.staging, &record.name)?
            .filter(|(identity, _)| identity.matches(&record.identity));
        let ours = |found: Option<(FileIdentity, u64)>| {
            found.is_some_and(|(identity, _)| identity.matches(&record.identity))
        };
        Ok(if ours(at_target) {
            if !is_staged(at_target, &record.identity, size) {
                return Ok(PublicationCheck::Damaged);
            }
            let parent = parent.expect("the target exists");
            sys::sync_dir(&parent).map_err(FsError::io("sync target directory"))?;
            if staged.is_some() {
                self.remove_staging_name(record)?;
            }
            PublicationCheck::Published
        } else if let Some((_, len)) = staged {
            if len < size {
                PublicationCheck::Damaged
            } else if at_target.is_some() {
                PublicationCheck::Conflict
            } else {
                PublicationCheck::NotPublished
            }
        } else {
            PublicationCheck::Lost
        })
    }

    /// Gives the published file `file` the name `to` in the same folder.
    /// The caller has recorded the rename first.
    ///
    /// The file must still be the one published, at its published size;
    /// anything else at the old name was put there outside Wayfetch and is
    /// left alone. The new name is hard-linked, which fails rather than
    /// replacing a file there, checked to be the published file, and the
    /// folder synchronized; only then is the old name removed, if it still
    /// holds the file, and the folder synchronized again. Any crash leaves
    /// a state [`Root::reconcile_rename`] can classify.
    pub fn rename_published(&self, file: &PublishedFile, to: &RelativePath) -> Result<(), FsError> {
        let (dir, from_name, to_name) = self.rename_names(file, to)?;
        if !is_staged(file_at(&dir, from_name)?, &file.identity, file.size) {
            return Err(self.published_problem(&dir, file)?);
        }
        crash_point!("rename-before-link");
        match dir.hard_link(from_name, &dir, to_name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(FsError::TargetExists(to.clone()));
            }
            Err(e) => return Err(FsError::io("link new name")(e)),
        }
        // The old name could have been swapped after it was checked; then
        // the name just created is not ours to keep.
        if !is_staged(file_at(&dir, to_name)?, &file.identity, file.size) {
            dir.remove_file(to_name)
                .map_err(FsError::io("remove mislinked name"))?;
            return Err(FsError::PublishedChanged(file.path.clone()));
        }
        sys::sync_dir(&dir).map_err(FsError::io("sync folder"))?;
        crash_point!("rename-before-unlink");
        self.remove_old_name(&dir, from_name, to_name, &file.identity)?;
        crash_point!("rename-before-commit");
        Ok(())
    }

    /// Classifies an interrupted rename of `file` to `to` after a restart.
    /// It is `Renamed` if the folder lists the new name, spelled exactly,
    /// and it holds the published file; the folder is then synchronized
    /// again, and the old name removed if it still holds the file. Nothing
    /// else is changed: a file that is not the published one is never
    /// deleted.
    ///
    /// On a case-insensitive or normalizing filesystem another spelling of
    /// the old name opens the old name's entry; it is not listed, so it is
    /// never taken for the new name.
    pub fn reconcile_rename(
        &self,
        file: &PublishedFile,
        to: &RelativePath,
    ) -> Result<RenameCheck, FsError> {
        let (dir, from_name, to_name) = self.rename_names(file, to)?;
        let renamed = listed(&dir, [to_name])? == [true]
            && file_at(&dir, to_name)?
                .is_some_and(|(identity, _)| identity.matches(&file.identity));
        if !renamed {
            return Ok(RenameCheck::NotRenamed);
        }
        sys::sync_dir(&dir).map_err(FsError::io("sync folder"))?;
        self.remove_old_name(&dir, from_name, to_name, &file.identity)?;
        Ok(RenameCheck::Renamed)
    }

    /// Deletes a target only while it still holds the recorded regular file.
    /// A durable intent permits missing names after interrupted deletion.
    /// External writers must be trusted, as for rename: portable filesystems
    /// provide no atomic compare-identity-and-unlink operation.
    pub fn delete_published(&self, file: &PublishedFile) -> Result<(), FsError> {
        self.verify()?;
        let (parent, name) = file.path.split_last();
        let dir = self
            .parent_dir(parent, false)?
            .ok_or_else(|| FsError::PublishedMissing(file.path.clone()))?;
        match dir.symlink_metadata(name) {
            Ok(meta) => {
                if !meta.is_file()
                    || !sys::identity(&meta)?.matches(&file.identity)
                    || meta.len() != file.size
                {
                    return Err(FsError::PublishedChanged(file.path.clone()));
                }
                dir.remove_file(name)
                    .map_err(FsError::io("delete downloaded file"))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(FsError::io("inspect downloaded file")(e)),
        }
        sys::sync_dir(&dir).map_err(FsError::io("sync download folder"))?;
        Ok(())
    }

    /// The absolute path of the published file `file`, for a host to open
    /// on its user's request, after checking that the root is mounted and
    /// the name still holds the file at its published size.
    pub fn published_path(&self, file: &PublishedFile) -> Result<PathBuf, FsError> {
        self.verify()?;
        let (parent, name) = file.path.split_last();
        let dir = self
            .parent_dir(parent, false)?
            .ok_or_else(|| FsError::PublishedMissing(file.path.clone()))?;
        if !is_staged(file_at(&dir, name)?, &file.identity, file.size) {
            return Err(self.published_problem(&dir, file)?);
        }
        Ok(self.path().join(file.path.as_str()))
    }

    /// The folder of a rename and its old and new names, after checking
    /// that the root is mounted and the new name is in the same folder.
    fn rename_names<'a>(
        &self,
        file: &'a PublishedFile,
        to: &'a RelativePath,
    ) -> Result<(Dir, &'a str, &'a str), FsError> {
        self.verify()?;
        let (parent, from_name) = file.path.split_last();
        let (to_parent, to_name) = to.split_last();
        if parent != to_parent {
            return Err(FsError::Unsupported("moving a file to another folder"));
        }
        let dir = self
            .parent_dir(parent, false)?
            .ok_or_else(|| FsError::PublishedMissing(file.path.clone()))?;
        Ok((dir, from_name, to_name))
    }

    /// Why the published file's name does not hold it.
    fn published_problem(&self, dir: &Dir, file: &PublishedFile) -> Result<FsError, FsError> {
        let (_, name) = file.path.split_last();
        Ok(match file_at(dir, name)? {
            None => FsError::PublishedMissing(file.path.clone()),
            Some(_) => FsError::PublishedChanged(file.path.clone()),
        })
    }

    /// Removes `name` from `dir` if it holds the file `identity`, and
    /// synchronizes the folder.
    /// Removes `name` if it holds the file whose new name is `new`. Only
    /// if the folder lists both names, spelled exactly, as two entries: on
    /// a case-insensitive or normalizing filesystem two spellings can open
    /// one entry, and removing one would remove the file.
    fn remove_old_name(
        &self,
        dir: &Dir,
        name: &str,
        new: &str,
        identity: &FileIdentity,
    ) -> Result<(), FsError> {
        let separate = name != new && listed(dir, [name, new])? == [true, true];
        let ours =
            separate && file_at(dir, name)?.is_some_and(|(found, _)| found.matches(identity));
        if ours {
            dir.remove_file(name)
                .map_err(FsError::io("remove old name"))?;
            sys::sync_dir(dir).map_err(FsError::io("sync folder"))?;
        }
        Ok(())
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
    pub(super) fn parent_dir(&self, parent: Option<&str>, create: bool) -> Result<Option<Dir>, FsError> {
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

/// Whether `found` is the staged file at exactly `size` bytes.
fn is_staged(found: Option<(FileIdentity, u64)>, expected: &FileIdentity, size: u64) -> bool {
    found.is_some_and(|(identity, len)| identity.matches(expected) && len == size)
}

/// Identity and length of whatever has `name` in `dir`, without following a
/// symlink.
/// Whether the folder lists an entry named exactly as each of `names`.
fn listed<const N: usize>(dir: &Dir, names: [&str; N]) -> Result<[bool; N], FsError> {
    let mut found = [false; N];
    for entry in dir.entries().map_err(FsError::io("list folder"))? {
        let name = entry.map_err(FsError::io("list folder"))?.file_name();
        for (seen, wanted) in found.iter_mut().zip(names) {
            *seen |= name == wanted;
        }
    }
    Ok(found)
}

fn file_at(dir: &Dir, name: &str) -> Result<Option<(FileIdentity, u64)>, FsError> {
    match dir.symlink_metadata(name) {
        Ok(meta) => Ok(Some((sys::identity(&meta)?, meta.len()))),
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
        root.publish(staging, &path("Movies/new/a.bin"), 8).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("Movies/new/a.bin")).unwrap(),
            b"artifact"
        );
        assert!(!staging_path(tmp.path(), &record.name).exists());
        assert_eq!(
            root.reconcile_publication(&record, &path("Movies/new/a.bin"), 8)
                .unwrap(),
            PublicationCheck::Published
        );

        // An unrelated file already at the target survives untouched.
        std::fs::write(tmp.path().join("b.bin"), b"user data").unwrap();
        let staging = staged(&root, 2, b"other");
        let record = staging.record().clone();
        assert!(matches!(
            root.publish(staging, &path("b.bin"), 5),
            Err(FsError::TargetExists(_))
        ));
        assert_eq!(
            std::fs::read(tmp.path().join("b.bin")).unwrap(),
            b"user data"
        );
        assert_eq!(
            root.reconcile_publication(&record, &path("b.bin"), 5)
                .unwrap(),
            PublicationCheck::Conflict
        );
        // The staged file is intact for another name.
        let staging = root.reopen_staging(&record, 5).unwrap();
        root.publish(staging, &path("b (1).bin"), 5).unwrap();
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
            root.reconcile_publication(&record, &target, 1).unwrap(),
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
            root.reconcile_publication(&record, &target, 1).unwrap(),
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
            root.reconcile_publication(&record, &path("other.bin"), 1)
                .unwrap(),
            PublicationCheck::Lost
        );
    }

    #[test]
    fn publication_holds_exactly_the_intended_bytes() {
        let (tmp, root) = root();
        // Bytes past the certified size never reach the artifact.
        let staging = staged(&root, 1, b"helloSTALE");
        let record = staging.record().clone();
        root.publish(staging, &path("a.bin"), 5).unwrap();
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"hello");
        assert_eq!(
            root.reconcile_publication(&record, &path("a.bin"), 5)
                .unwrap(),
            PublicationCheck::Published
        );

        // A published file truncated or extended later is not complete.
        for len in [0, 10] {
            std::fs::File::options()
                .write(true)
                .open(tmp.path().join("a.bin"))
                .unwrap()
                .set_len(len)
                .unwrap();
            assert_eq!(
                root.reconcile_publication(&record, &path("a.bin"), 5)
                    .unwrap(),
                PublicationCheck::Damaged
            );
        }

        // A staging file shorter than the intent is refused, not published.
        let staging = staged(&root, 2, b"abc");
        let record = staging.record().clone();
        assert!(matches!(
            root.publish(staging, &path("b.bin"), 5),
            Err(FsError::StagingShrunk {
                len: 3,
                needed: 5,
                ..
            })
        ));
        assert!(!tmp.path().join("b.bin").exists());
        assert_eq!(
            root.reconcile_publication(&record, &path("b.bin"), 5)
                .unwrap(),
            PublicationCheck::Damaged
        );
    }

    #[test]
    fn reconcile_syncs_the_target_directory_before_removing_staging() {
        let (tmp, root) = root();
        let staging = staged(&root, 1, b"x");
        let record = staging.record().clone();
        drop(staging);
        // Crash after the link, before the target directory was synced.
        std::fs::create_dir(tmp.path().join("dir")).unwrap();
        std::fs::hard_link(
            staging_path(tmp.path(), &record.name),
            tmp.path().join("dir/a.bin"),
        )
        .unwrap();
        let target_dir = root.parent_dir(Some("dir"), false).unwrap().unwrap();
        let target_dir = sys::identity(&target_dir.dir_metadata().unwrap()).unwrap();
        sys::tests::take_synced();
        assert_eq!(
            root.reconcile_publication(&record, &path("dir/a.bin"), 1)
                .unwrap(),
            PublicationCheck::Published
        );
        let synced = sys::tests::take_synced();
        let staging_dir = sys::identity(&root.staging.dir_metadata().unwrap()).unwrap();
        let target_at = synced.iter().position(|d| d.matches(&target_dir));
        let staging_at = synced.iter().position(|d| d.matches(&staging_dir));
        // The target directory is durable before the staging name goes.
        assert!(
            matches!((target_at, staging_at), (Some(t), Some(s)) if t < s),
            "{synced:?}"
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
            root.reconcile_publication(&record, &path("a.bin"), 1),
            Err(FsError::SentinelMissing(_))
        ));
    }

    /// Publishes `bytes` at `at` and returns the published file.
    pub(super) fn published(root: &Root, at: &str, bytes: &[u8]) -> PublishedFile {
        let staging = staged(root, 1, bytes);
        let identity = staging.record().identity;
        root.publish(staging, &path(at), bytes.len() as u64)
            .unwrap();
        PublishedFile {
            path: path(at),
            identity,
            size: bytes.len() as u64,
        }
    }

    #[test]
    fn renames_without_clobbering() {
        let (tmp, root) = root();
        let file = published(&root, "dir/a.bin", b"data");
        std::fs::write(tmp.path().join("dir/taken.bin"), b"user").unwrap();
        assert!(matches!(
            root.rename_published(&file, &path("dir/taken.bin")),
            Err(FsError::TargetExists(_))
        ));
        assert_eq!(
            std::fs::read(tmp.path().join("dir/taken.bin")).unwrap(),
            b"user"
        );
        assert!(matches!(
            root.rename_published(&file, &path("elsewhere/b.bin")),
            Err(FsError::Unsupported(_))
        ));
        root.rename_published(&file, &path("dir/b.bin")).unwrap();
        assert!(!tmp.path().join("dir/a.bin").exists());
        assert_eq!(
            std::fs::read(tmp.path().join("dir/b.bin")).unwrap(),
            b"data"
        );
        assert_eq!(
            root.reconcile_rename(&file, &path("dir/b.bin")).unwrap(),
            RenameCheck::Renamed
        );
    }

    #[test]
    fn a_file_replaced_outside_wayfetch_is_not_renamed() {
        let (tmp, root) = root();
        let file = published(&root, "a.bin", b"data");
        // Replaced by another file of the same size.
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
        std::fs::write(tmp.path().join("a.bin"), b"evil").unwrap();
        assert!(matches!(
            root.rename_published(&file, &path("b.bin")),
            Err(FsError::PublishedChanged(_))
        ));
        assert!(!tmp.path().join("b.bin").exists());
        assert!(matches!(
            root.published_path(&file),
            Err(FsError::PublishedChanged(_))
        ));
        std::fs::remove_file(tmp.path().join("a.bin")).unwrap();
        assert!(matches!(
            root.rename_published(&file, &path("b.bin")),
            Err(FsError::PublishedMissing(_))
        ));

        // The published file, changed in size.
        let file = published(&root, "c.bin", b"data");
        std::fs::write(tmp.path().join("c.bin"), b"longer").unwrap();
        assert!(matches!(
            root.rename_published(&file, &path("d.bin")),
            Err(FsError::PublishedChanged(_))
        ));
    }

    #[test]
    fn reconciles_each_rename_step() {
        let (tmp, root) = root();
        let file = published(&root, "a.bin", b"data");
        let to = path("b.bin");
        // Recorded, nothing linked yet.
        assert_eq!(
            root.reconcile_rename(&file, &to).unwrap(),
            RenameCheck::NotRenamed
        );
        assert!(tmp.path().join("a.bin").exists());
        // Linked, old name not yet removed.
        std::fs::hard_link(tmp.path().join("a.bin"), tmp.path().join("b.bin")).unwrap();
        assert_eq!(
            root.reconcile_rename(&file, &to).unwrap(),
            RenameCheck::Renamed
        );
        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(std::fs::read(tmp.path().join("b.bin")).unwrap(), b"data");
        // A file that took the old name meanwhile is left alone.
        std::fs::write(tmp.path().join("a.bin"), b"user").unwrap();
        assert_eq!(
            root.reconcile_rename(&file, &to).unwrap(),
            RenameCheck::Renamed
        );
        assert_eq!(std::fs::read(tmp.path().join("a.bin")).unwrap(), b"user");
        // An unrelated file at the new name is not the rename.
        let other = published(&root, "c.bin", b"more");
        std::fs::write(tmp.path().join("d.bin"), b"user").unwrap();
        assert_eq!(
            root.reconcile_rename(&other, &path("d.bin")).unwrap(),
            RenameCheck::NotRenamed
        );
        assert!(tmp.path().join("c.bin").exists());
    }

    /// Case-insensitive filesystems cannot be made here, so the guard is
    /// checked directly: the old name goes only while the folder lists the
    /// new name, spelled exactly, as another entry.
    #[test]
    fn the_old_name_stays_unless_the_new_one_is_another_entry() {
        let (tmp, root) = root();
        let file = published(&root, "a.bin", b"data");
        let dir = root.dir.try_clone().unwrap();
        assert_eq!(listed(&dir, ["a.bin", "A.BIN"]).unwrap(), [true, false]);
        for new in ["A.BIN", "a.bin"] {
            root.remove_old_name(&dir, "a.bin", new, &file.identity)
                .unwrap();
            assert!(tmp.path().join("a.bin").exists(), "{new}");
        }
        std::fs::hard_link(tmp.path().join("a.bin"), tmp.path().join("A.BIN")).unwrap();
        root.remove_old_name(&dir, "a.bin", "A.BIN", &file.identity)
            .unwrap();
        assert!(!tmp.path().join("a.bin").exists());
        assert_eq!(std::fs::read(tmp.path().join("A.BIN")).unwrap(), b"data");
    }

    #[test]
    fn published_paths_are_checked() {
        let (tmp, root) = root();
        let file = published(&root, "dir/a.bin", b"data");
        assert_eq!(
            root.published_path(&file).unwrap(),
            tmp.path().join("dir/a.bin")
        );
        std::fs::remove_dir_all(tmp.path().join(crate::fs::PRIVATE_DIR)).unwrap();
        assert!(matches!(
            root.published_path(&file),
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
        assert!(root.publish(staging, &path("escape/a.bin"), 1).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod deletion_tests {
    use super::tests::published;
    use super::*;
    use crate::fs::staging::tests::root;

    #[test]
    fn delete_checks_identity_type_size_and_synchronizes_missing_names() {
        let (tmp, root) = root();
        let file = published(&root, "owned.bin", b"owned");
        std::fs::write(tmp.path().join("unrelated.bin"), b"user").unwrap();
        std::fs::write(tmp.path().join("replacement.bin"), b"owned").unwrap();
        std::fs::rename(
            tmp.path().join("replacement.bin"),
            tmp.path().join("owned.bin"),
        )
        .unwrap();
        assert!(matches!(
            root.delete_published(&file),
            Err(FsError::PublishedChanged(_))
        ));
        assert_eq!(
            std::fs::read(tmp.path().join("owned.bin")).unwrap(),
            b"owned"
        );
        std::fs::remove_file(tmp.path().join("owned.bin")).unwrap();
        root.delete_published(&file).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("unrelated.bin")).unwrap(),
            b"user"
        );
        let file = published(&root, "size.bin", b"original");
        std::fs::write(tmp.path().join("size.bin"), b"changed size").unwrap();
        assert!(matches!(
            root.delete_published(&file),
            Err(FsError::PublishedChanged(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn delete_never_follows_symlinks_or_an_unmounted_root() {
        let (tmp, root) = root();
        let file = published(&root, "owned.bin", b"owned");
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();
        std::fs::remove_file(tmp.path().join("owned.bin")).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("owned.bin")).unwrap();
        assert!(root.delete_published(&file).is_err());
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"outside");
        // Move the opened root away and put another directory at its path.
        let mounted = tmp.path().with_extension("mounted");
        std::fs::rename(tmp.path(), &mounted).unwrap();
        std::fs::create_dir(tmp.path()).unwrap();
        assert!(root.delete_published(&file).is_err());
        std::fs::remove_dir(tmp.path()).unwrap();
        std::fs::rename(mounted, tmp.path()).unwrap();
    }
}
