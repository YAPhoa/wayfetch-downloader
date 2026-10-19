//! The filesystem side of unpacking an archive (architecture brief
//! sections 9.3 to 9.5): a private folder per attempt under
//! `.wayfetch/extract/` in the archive's root, checking what was unpacked
//! there, and moving it next to the archive without replacing anything.

use std::io;
use std::path::PathBuf;

use cap_std::fs::{Dir, Metadata};

use super::path::PRIVATE_DIR;
use super::{FsError, RelativePath, Root, sys};
use crate::extract::MAX_ENTRIES;

/// The folder of private extraction folders, inside [`PRIVATE_DIR`].
const EXTRACT: &str = "extract";

/// Deepest folder nesting an unpacked tree may have.
const MAX_TREE_DEPTH: usize = 128;

/// Free space kept on the root's filesystem while unpacking.
pub const RESERVE_BYTES: u64 = 256 << 20;

/// What a private extraction folder holds so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Bytes in its files.
    pub bytes: u64,
    /// Files and folders.
    pub entries: usize,
}

/// Why an unpacked tree cannot be published.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// It holds something other than plain files and folders, or is too
    /// big; the message says what, for the person.
    #[error("{0}")]
    Refused(String),
    /// The filesystem failed.
    #[error(transparent)]
    Fs(#[from] FsError),
}

impl Root {
    /// A new, empty private folder `name` for an extraction to write into,
    /// replacing any earlier one of that name, as an absolute path for the
    /// extractor.
    pub fn create_extraction(&self, name: &str) -> Result<PathBuf, FsError> {
        self.verify()?;
        let private = self
            .dir
            .open_dir(PRIVATE_DIR)
            .map_err(FsError::io("open private directory"))?;
        match private.create_dir(EXTRACT) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(FsError::io("create extraction directory")(e)),
        }
        let extract = private
            .open_dir(EXTRACT)
            .map_err(FsError::io("open extraction directory"))?;
        remove_tree(&extract, name)?;
        extract
            .create_dir(name)
            .map_err(FsError::io("create extraction folder"))?;
        Ok(self.path().join(PRIVATE_DIR).join(EXTRACT).join(name))
    }

    /// Removes the private extraction folder `name` and everything in it,
    /// if it exists. Links in it are removed, never followed.
    pub fn remove_extraction(&self, name: &str) -> Result<(), FsError> {
        match self.extractions()? {
            Some(extract) => remove_tree(&extract, name),
            None => Ok(()),
        }
    }

    /// Counts what the private folder `name` holds, stopping early once it
    /// passes `budget` bytes or [`MAX_ENTRIES`]. Also says how many bytes
    /// are free on the root's filesystem, where it can tell.
    pub fn tally_extraction(
        &self,
        name: &str,
        budget: u64,
    ) -> Result<(Tally, Option<u64>), FsError> {
        let dir = self.extraction(name)?;
        let mut tally = Tally::default();
        walk(&dir, 0, &mut |_, _, meta| {
            tally.entries += 1;
            if meta.is_file() {
                tally.bytes = tally.bytes.saturating_add(meta.len());
            }
            Ok(tally.bytes <= budget && tally.entries <= MAX_ENTRIES)
        })
        .map_err(|e| match e {
            TreeError::Fs(e) => e,
            TreeError::Refused(_) => FsError::Unsupported("folder too deep"),
        })?;
        Ok((tally, free_bytes(&dir)))
    }

    /// Checks the unpacked tree in the private folder `name` before it is
    /// published: only plain files, each with one name, and folders, at
    /// most `budget` bytes and [`MAX_ENTRIES`] entries. Permission bits the
    /// archive set beyond the owner's, group's, and others' read, write,
    /// and execute (set-user-ID and the like) are cleared, and the owner
    /// can always read, change, and remove what was unpacked.
    pub fn check_extraction(&self, name: &str, budget: u64) -> Result<Tally, TreeError> {
        use cap_std::fs::{MetadataExt, PermissionsExt};
        let dir = self.extraction(name)?;
        let mut tally = Tally::default();
        walk(&dir, 0, &mut |parent, entry, meta| {
            tally.entries += 1;
            if tally.entries > MAX_ENTRIES {
                return Err(TreeError::Refused(format!(
                    "The archive holds more than {MAX_ENTRIES} files."
                )));
            }
            let owner = if meta.is_dir() {
                0o700
            } else if meta.is_file() && meta.nlink() == 1 {
                tally.bytes = tally.bytes.saturating_add(meta.len());
                0o600
            } else if meta.is_file() {
                return Err(TreeError::Refused(
                    "The archive holds hard links, which are not unpacked.".into(),
                ));
            } else if meta.file_type().is_symlink() {
                return Err(TreeError::Refused(
                    "The archive holds links, which are not unpacked.".into(),
                ));
            } else {
                return Err(TreeError::Refused(
                    "The archive holds devices or pipes, which are not unpacked.".into(),
                ));
            };
            if tally.bytes > budget {
                return Err(TreeError::Refused(
                    "The archive unpacks to more than its listing says.".into(),
                ));
            }
            let mode = meta.mode();
            let wanted = (mode & 0o777) | owner;
            if mode & 0o7777 != wanted {
                let mut permissions = meta.permissions();
                permissions.set_mode(wanted);
                parent
                    .set_permissions(entry, permissions)
                    .map_err(FsError::io("set unpacked permissions"))?;
            }
            Ok(true)
        })?;
        Ok(tally)
    }

    /// Moves what the private folder `name` holds next to the archive, in
    /// `parent` (the root itself if `None`), without replacing anything:
    /// its only entry under that entry's own name, or else the whole
    /// folder named `stem`, taking `Name (2)` and so on when a name is
    /// taken. Answers where it went.
    pub fn publish_extraction(
        &self,
        name: &str,
        parent: Option<&str>,
        stem: &str,
    ) -> Result<RelativePath, FsError> {
        self.verify()?;
        let extract = self
            .extractions()?
            .ok_or_else(|| FsError::StagingMissing(name.to_owned()))?;
        let staged = extract
            .open_dir(name)
            .map_err(FsError::io("open extraction folder"))?;
        let mut top = Vec::new();
        for entry in staged
            .entries()
            .map_err(FsError::io("list extraction folder"))?
        {
            let entry = entry.map_err(FsError::io("list extraction folder"))?;
            top.push(entry.file_name());
            if top.len() > 1 {
                break;
            }
        }
        let target = self
            .parent_dir(parent, false)?
            .ok_or_else(|| FsError::io("open archive folder")(io::ErrorKind::NotFound.into()))?;
        // Its only entry, if its name is one every platform accepts.
        let single = match top.as_slice() {
            [only] => only
                .to_str()
                .filter(|n| RelativePath::new(n).is_ok())
                .map(str::to_owned),
            _ => None,
        };
        let (from_dir, from, wanted, is_dir) = match &single {
            Some(only) => {
                let is_dir = staged
                    .symlink_metadata(only)
                    .map_err(FsError::io("inspect unpacked entry"))?
                    .is_dir();
                (&staged, only.as_str(), only.as_str(), is_dir)
            }
            None => (&extract, name, stem, true),
        };
        for n in 0..1000u32 {
            let candidate = numbered(wanted, n, is_dir);
            let path = match parent {
                Some(parent) => RelativePath::new(&format!("{parent}/{candidate}"))?,
                None => RelativePath::new(&candidate)?,
            };
            let placed = if is_dir {
                // Creating the name claims it; a rename then replaces only
                // that empty folder.
                match target.create_dir(&candidate) {
                    Ok(()) => from_dir
                        .rename(from, &target, &candidate)
                        .map_err(FsError::io("move unpacked folder")),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => Err(FsError::io("claim unpacked folder name")(e)),
                }
            } else {
                match from_dir.hard_link(from, &target, &candidate) {
                    Ok(()) => from_dir
                        .remove_file(from)
                        .map_err(FsError::io("remove unpacked name")),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => Err(FsError::io("link unpacked file")(e)),
                }
            };
            placed?;
            sys::sync_dir(&target).map_err(FsError::io("sync archive folder"))?;
            if single.is_some() {
                // The private folder is empty now. Left behind, it only
                // takes a name in the root's private directory.
                let _ = extract.remove_dir(name);
            }
            return Ok(path);
        }
        Err(FsError::TargetExists(RelativePath::new(wanted)?))
    }

    /// The root's folder of private extraction folders, if it exists.
    fn extractions(&self) -> Result<Option<Dir>, FsError> {
        match self
            .dir
            .open_dir(std::path::Path::new(PRIVATE_DIR).join(EXTRACT))
        {
            Ok(dir) => Ok(Some(dir)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FsError::io("open extraction directory")(e)),
        }
    }

    /// The private extraction folder `name`.
    fn extraction(&self, name: &str) -> Result<Dir, FsError> {
        self.extractions()?
            .ok_or_else(|| FsError::StagingMissing(name.to_owned()))?
            .open_dir(name)
            .map_err(FsError::io("open extraction folder"))
    }
}

/// `name`, or its `n`th alternative: `Name (n)` for a folder, and before
/// the extension for a file.
fn numbered(name: &str, n: u32, is_dir: bool) -> String {
    match (n, is_dir, name.rfind('.').filter(|&dot| dot > 0)) {
        (0, ..) => name.to_owned(),
        (_, false, Some(dot)) => format!("{} ({}){}", &name[..dot], n + 1, &name[dot..]),
        _ => format!("{name} ({})", n + 1),
    }
}

/// Removes `name` in `dir` and everything below it, if it exists.
fn remove_tree(dir: &Dir, name: &str) -> Result<(), FsError> {
    match dir.remove_dir_all(name) {
        Ok(()) => sys::sync_dir(dir).map_err(FsError::io("sync extraction directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(FsError::io("remove extraction folder")(e)),
    }
}

/// Calls `visit` with each entry below `dir`, its folder, and its
/// metadata, without following links, folders before what they hold.
/// Stops when `visit` answers `false`.
fn walk(
    dir: &Dir,
    depth: usize,
    visit: &mut dyn FnMut(&Dir, &std::ffi::OsStr, &Metadata) -> Result<bool, TreeError>,
) -> Result<bool, TreeError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TreeError::Refused(
            "The archive's folders are nested too deeply.".into(),
        ));
    }
    for entry in dir.entries().map_err(FsError::io("list unpacked folder"))? {
        let name = entry
            .map_err(FsError::io("list unpacked folder"))?
            .file_name();
        let meta = dir
            .symlink_metadata(&name)
            .map_err(FsError::io("inspect unpacked entry"))?;
        if !visit(dir, &name, &meta)? {
            return Ok(false);
        }
        if meta.is_dir() {
            let child = dir
                .open_dir(&name)
                .map_err(FsError::io("open unpacked folder"))?;
            if !walk(&child, depth + 1, visit)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Bytes free for an unprivileged writer on `dir`'s filesystem.
#[cfg(unix)]
fn free_bytes(dir: &Dir) -> Option<u64> {
    let stat = rustix::fs::fstatvfs(dir).ok()?;
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(not(unix))]
fn free_bytes(_dir: &Dir) -> Option<u64> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::fs::staging::tests::root;

    fn fill(path: &std::path::Path, files: &[(&str, &[u8])]) {
        for (name, data) in files {
            let file = path.join(name);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, data).unwrap();
        }
    }

    #[test]
    fn a_tree_of_several_entries_becomes_a_folder_named_for_the_archive() {
        let (tmp, root) = root();
        let staging = root.create_extraction("7-g1").unwrap();
        fill(&staging, &[("a.txt", b"hello"), ("d/b.txt", b"bye")]);
        assert_eq!(
            root.tally_extraction("7-g1", u64::MAX).unwrap().0,
            Tally {
                bytes: 8,
                entries: 3
            }
        );
        assert_eq!(root.check_extraction("7-g1", 100).unwrap().bytes, 8);
        std::fs::create_dir(tmp.path().join("Game")).unwrap();
        let path = root.publish_extraction("7-g1", None, "Game").unwrap();
        assert_eq!(path.as_str(), "Game (2)");
        assert_eq!(
            std::fs::read(tmp.path().join("Game (2)/d/b.txt")).unwrap(),
            b"bye"
        );
        assert!(!staging.exists());
        // The name taken first was left alone.
        assert_eq!(
            std::fs::read_dir(tmp.path().join("Game")).unwrap().count(),
            0
        );
    }

    #[test]
    fn a_single_entry_keeps_its_own_name_beside_the_archive() {
        let (tmp, root) = root();
        std::fs::create_dir(tmp.path().join("Movies")).unwrap();
        let staging = root.create_extraction("8-g2").unwrap();
        fill(&staging, &[("film.mkv", b"x")]);
        root.check_extraction("8-g2", 10).unwrap();
        std::fs::write(tmp.path().join("Movies/film.mkv"), b"mine").unwrap();
        let path = root
            .publish_extraction("8-g2", Some("Movies"), "film")
            .unwrap();
        assert_eq!(path.as_str(), "Movies/film (2).mkv");
        assert_eq!(
            std::fs::read(tmp.path().join("Movies/film.mkv")).unwrap(),
            b"mine"
        );

        let staging = root.create_extraction("8-g3").unwrap();
        fill(&staging, &[("Game/data.bin", b"d")]);
        root.check_extraction("8-g3", 10).unwrap();
        let path = root.publish_extraction("8-g3", None, "x").unwrap();
        assert_eq!(path.as_str(), "Game");
        root.remove_extraction("8-g3").unwrap();
    }

    #[test]
    fn links_devices_and_oversized_trees_are_refused_and_bits_cleared() {
        let (_tmp, root) = root();
        let staging = root.create_extraction("1-g1").unwrap();
        fill(&staging, &[("a", b"12345")]);
        std::os::unix::fs::symlink("/etc/passwd", staging.join("link")).unwrap();
        let refused = root.check_extraction("1-g1", 100).unwrap_err();
        assert!(refused.to_string().contains("links"), "{refused}");

        let staging = root.create_extraction("1-g1").unwrap();
        fill(&staging, &[("a", b"12345")]);
        std::fs::hard_link(staging.join("a"), staging.join("b")).unwrap();
        assert!(root.check_extraction("1-g1", 100).is_err());

        let staging = root.create_extraction("1-g1").unwrap();
        fill(&staging, &[("a", b"12345")]);
        assert!(root.check_extraction("1-g1", 4).is_err());
        assert!(root.tally_extraction("1-g1", 4).unwrap().0.bytes > 4);

        std::fs::set_permissions(
            staging.join("a"),
            std::fs::Permissions::from_mode(0o4000 | 0o400),
        )
        .unwrap();
        root.check_extraction("1-g1", 100).unwrap();
        let mode = std::fs::metadata(staging.join("a"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o600);

        root.remove_extraction("1-g1").unwrap();
        assert!(!staging.exists());
        root.remove_extraction("1-g1").unwrap();
    }

    #[test]
    fn numbered_names_keep_extensions_on_files_only() {
        assert_eq!(numbered("a.b", 0, true), "a.b");
        assert_eq!(numbered("Game.v1", 1, true), "Game.v1 (2)");
        assert_eq!(numbered("film.mkv", 2, false), "film (3).mkv");
        assert_eq!(numbered(".rc", 1, false), ".rc (2)");
    }
}
