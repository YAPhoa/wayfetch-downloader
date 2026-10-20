//! Unpacking downloaded archives (architecture brief sections 3.12 and 9,
//! ADR-18 and ADR-19; protocol 6.0).
//!
//! The engine owns the workflow: which downloads are archives ([`names`]),
//! when a set of volumes is ready, one extraction at a time, where its
//! output goes, its private staging folder, the checks before and after,
//! publishing the result next to the archive, cancelling, and restarting
//! after a crash. An [`Extractor`], such as the 7-Zip plugin, only lists
//! and unpacks, as a separate program the engine can stop at any moment.
//!
//! An extraction never changes or removes its archive. Its output is
//! written into a new private folder under the root's `.wayfetch`
//! directory, checked to hold only plain files and folders within the
//! extraction's budget, and only then moved next to the archive under a
//! free name. A failed, cancelled, or interrupted extraction leaves
//! nothing but its private folder, which is removed.
//!
//! Hostile names are refused from the listing, the output is checked
//! again after unpacking, and its size is watched while it is written. The
//! extractor itself must also be confined to the archive's volumes and the
//! private folder (section 9.3), as the 7-Zip plugin is: the engine cannot
//! confine it, so an extractor that cannot be confined reports itself not
//! ready, and nothing is unpacked.

pub mod names;

use std::path::{Path, PathBuf};

use wayfetch_protocol::media::PluginSummary;

use crate::accounts::Secret;
use crate::transfer::writer::BoxFuture;
pub use names::{ArchiveName, Layout};

/// Most entries one archive may hold.
pub const MAX_ENTRIES: usize = 100_000;

/// What an archive holds, as its listing says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its path in the archive, as listed.
    pub path: String,
    /// What it is.
    pub kind: EntryKind,
    /// Its unpacked size, if listed.
    pub size: Option<u64>,
    /// Whether its contents need a password.
    pub encrypted: bool,
}

/// What an archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A plain file.
    File,
    /// A folder.
    Directory,
    /// A symbolic or hard link.
    Link,
    /// A device, pipe, or socket.
    Special,
}

/// Why listing or unpacking an archive failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtractError {
    /// The archive is protected and no password was given.
    #[error("the archive needs a password")]
    NeedsPassword,
    /// The password given is not the archive's.
    #[error("the password is wrong")]
    WrongPassword,
    /// A volume of the archive is missing.
    #[error("a part of the archive is missing")]
    MissingVolume,
    /// The extractor cannot read this kind of archive.
    #[error("{0}")]
    Unsupported(String),
    /// The archive is damaged or not an archive.
    #[error("{0}")]
    Damaged(String),
    /// The extractor failed some other way.
    #[error("{0}")]
    Failed(String),
    /// It was halted, and nothing it started still runs.
    #[error("unpacking was stopped")]
    Stopped,
}

/// An archive to list or unpack.
#[derive(Debug, Clone, Copy)]
pub struct Archive<'a> {
    /// Its first volume.
    pub path: &'a Path,
    /// Its other volumes, next to it: all that the extractor may read
    /// besides the first.
    pub volumes: &'a [PathBuf],
    /// How it is unpacked.
    pub layout: Layout,
    /// Its password, if one was given.
    pub password: Option<&'a Secret>,
}

/// Resolves when an extractor's run must stop. The run then stops all it
/// started and returns [`ExtractError::Stopped`] only once none of it can
/// still write, so its output folder can be removed. Dropping a run's
/// future instead stops it without that wait.
pub type Halt = BoxFuture<'static, ()>;

/// Lists and unpacks archives.
pub trait Extractor: Send + Sync + 'static {
    /// The plugin, and whether it can be used.
    fn plugin(&self) -> PluginSummary;

    /// Whether it can be used.
    fn ready(&self) -> bool {
        self.plugin().usable()
    }

    /// The entries of `archive`, at most [`MAX_ENTRIES`]; more is an
    /// error. Stops when `halt` resolves (see [`Halt`]).
    fn list<'a>(
        &'a self,
        archive: Archive<'a>,
        halt: Halt,
    ) -> BoxFuture<'a, Result<Vec<Entry>, ExtractError>>;

    /// Unpacks `archive` into the empty folder `output`, writing no file
    /// larger than `budget` bytes. Stops when `halt` resolves (see
    /// [`Halt`]).
    fn extract<'a>(
        &'a self,
        archive: Archive<'a>,
        output: &'a Path,
        budget: u64,
        halt: Halt,
    ) -> BoxFuture<'a, Result<(), ExtractError>>;
}

/// What a listing allows: the total size its entries declare, if every
/// file declares one, or why it is refused. Absolute paths, `..`, links,
/// devices, and more than [`MAX_ENTRIES`] entries are refused; whether a
/// password is needed is the extractor's to say.
pub fn check_listing(entries: &[Entry]) -> Result<Option<u64>, String> {
    if entries.len() > MAX_ENTRIES {
        return Err(format!("The archive holds more than {MAX_ENTRIES} files."));
    }
    let mut total = Some(0u64);
    for entry in entries {
        let path = &entry.path;
        let unsafe_path = path.is_empty()
            || path.starts_with(['/', '\\'])
            || matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
            || path.split(['/', '\\']).any(|c| c == "..");
        if unsafe_path {
            return Err("The archive names a file outside its own folder.".into());
        }
        match entry.kind {
            EntryKind::File => {
                total = total.zip(entry.size).and_then(|(t, s)| t.checked_add(s));
            }
            EntryKind::Directory => {}
            EntryKind::Link => {
                return Err("The archive holds links, which are not unpacked.".into());
            }
            EntryKind::Special => {
                return Err("The archive holds devices or pipes, which are not unpacked.".into());
            }
        }
    }
    Ok(total)
}

/// The most an extraction may write: what its listing declares with some
/// room for rounding, or, when it declares nothing (a compressed tar), a
/// generous multiple of the archive's own size, so a decompression bomb
/// stops early.
pub fn budget(declared: Option<u64>, archive_bytes: u64) -> u64 {
    const SLACK: u64 = 1 << 20;
    match declared {
        Some(total) => total.saturating_add(total / 100).saturating_add(SLACK),
        None => archive_bytes.saturating_mul(1000).max(1 << 30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: EntryKind, size: Option<u64>) -> Entry {
        Entry {
            path: path.into(),
            kind,
            size,
            encrypted: false,
        }
    }

    #[test]
    fn listings_with_escaping_names_or_links_are_refused() {
        let ok = [
            entry("a.txt", EntryKind::File, Some(6)),
            entry("d", EntryKind::Directory, None),
            entry("d/b..c.txt", EntryKind::File, Some(4)),
        ];
        assert_eq!(check_listing(&ok), Ok(Some(10)));
        let unknown = [entry("a", EntryKind::File, None)];
        assert_eq!(check_listing(&unknown), Ok(None));
        for bad in [
            "/etc/passwd",
            "../x",
            "d/../../x",
            "C:x",
            "\\x",
            "a\\..\\..\\x",
            "",
        ] {
            let listing = [entry(bad, EntryKind::File, Some(1))];
            assert!(check_listing(&listing).is_err(), "{bad}");
        }
        for kind in [EntryKind::Link, EntryKind::Special] {
            assert!(check_listing(&[entry("x", kind, Some(1))]).is_err());
        }
        let many = vec![entry("x", EntryKind::File, Some(0)); MAX_ENTRIES + 1];
        assert!(check_listing(&many).is_err());
    }

    #[test]
    fn budgets_follow_the_listing_or_the_archive_size() {
        assert_eq!(budget(Some(100 << 20), 5), (101 << 20) + (1 << 20));
        assert_eq!(budget(None, 10), 1 << 30);
        assert_eq!(budget(None, 10 << 20), 10_000 << 20);
        assert_eq!(budget(Some(u64::MAX), 1), u64::MAX);
    }
}
