//! Which downloaded files are archives, and which belong to one set of
//! volumes, judged by their names (architecture brief section 9.2).
//!
//! Names are classified conservatively: only the patterns below count, and
//! a set is never assumed complete from names alone. The extractor still
//! checks the volumes it finds, so a part that is missing when unpacking
//! starts is reported then.

/// How an archive is unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// The extractor reads it directly: ZIP, 7z, RAR, a plain tar, or a
    /// single compressed file.
    Direct,
    /// A tar inside a compressed stream (`.tar.gz` and the like): the
    /// stream is decompressed, then the tar unpacked.
    CompressedTar,
}

/// A file name recognized as an archive or one volume of one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveName {
    /// What every volume of the set shares, compared without case.
    set: String,
    /// Whether this is the volume unpacking starts from.
    pub first: bool,
    /// What unpacking makes when the archive holds several entries: its
    /// name without the archive extensions.
    pub stem: String,
    /// How it is unpacked.
    pub layout: Layout,
}

impl ArchiveName {
    /// `name` as an archive, if it is one. `name` is one path component.
    pub fn parse(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        let make = |family: &str, stem_len: usize, first: bool, layout: Layout| {
            let stem = &name[..stem_len];
            (!stem.is_empty()).then(|| Self {
                set: format!("{family}:{}", stem.to_lowercase()),
                first,
                stem: stem.to_owned(),
                layout,
            })
        };
        // `name.part3.rar`: RAR volumes numbered from 1.
        if let Some(rest) = lower.strip_suffix(".rar")
            && let Some((stem, number)) = rest.rsplit_once(".part")
            && let Some(n) = number_of(number)
        {
            return make("rar", stem.len(), n == 1, Layout::Direct);
        }
        // `name.7z.001`: any archive split into numbered pieces.
        if let Some((rest, number)) = lower.rsplit_once('.')
            && number.len() == 3
            && let Some(n) = number_of(number)
            && let Some((stem, ext)) = rest.rsplit_once('.')
            && matches!(ext, "7z" | "zip" | "rar" | "tar")
        {
            return make("split", stem.len(), n == 1, Layout::Direct);
        }
        // `name.r00` after `name.rar`, and `name.z01` before `name.zip`.
        if let Some((stem, ext)) = lower.rsplit_once('.')
            && ext.len() == 3
            && number_of(&ext[1..]).is_some()
        {
            match &ext[..1] {
                "r" => return make("rar-old", stem.len(), false, Layout::Direct),
                "z" => return make("zip", stem.len(), false, Layout::Direct),
                _ => {}
            }
        }
        for (suffix, family, layout) in [
            (".rar", "rar-old", Layout::Direct),
            (".zip", "zip", Layout::Direct),
            (".7z", "7z", Layout::Direct),
            (".tar", "tar", Layout::Direct),
            (".tar.gz", "tar", Layout::CompressedTar),
            (".tgz", "tar", Layout::CompressedTar),
            (".tar.xz", "tar", Layout::CompressedTar),
            (".txz", "tar", Layout::CompressedTar),
            (".tar.bz2", "tar", Layout::CompressedTar),
            (".tbz2", "tar", Layout::CompressedTar),
            (".tbz", "tar", Layout::CompressedTar),
            (".tar.zst", "tar", Layout::CompressedTar),
            (".tzst", "tar", Layout::CompressedTar),
        ] {
            if lower.ends_with(suffix) {
                return make(family, name.len() - suffix.len(), true, layout);
            }
        }
        None
    }

    /// Whether `other` is a volume of the same set.
    pub fn same_set(&self, other: &Self) -> bool {
        self.set == other.set
    }
}

/// `digits` as a number, if it is only ASCII digits.
fn number_of(digits: &str) -> Option<u32> {
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(name: &str) -> Option<(bool, String, Layout)> {
        ArchiveName::parse(name).map(|a| (a.first, a.stem, a.layout))
    }

    #[test]
    fn single_archives_are_their_own_first_volume() {
        for (name, stem) in [
            ("Game.zip", "Game"),
            ("Game.7z", "Game"),
            ("Game.RAR", "Game"),
            ("src.tar", "src"),
        ] {
            assert_eq!(parsed(name), Some((true, stem.into(), Layout::Direct)));
        }
        for name in ["src.tar.gz", "src.tgz", "src.tar.xz", "src.tar.zst", "src.tbz2"] {
            assert_eq!(
                parsed(name),
                Some((true, "src".into(), Layout::CompressedTar)),
                "{name}"
            );
        }
        for name in ["movie.mkv", "notes.txt", ".zip", "a.part1.txt", "a.gz", "a.r0x"] {
            assert_eq!(parsed(name), None, "{name}");
        }
    }

    #[test]
    fn volumes_are_grouped_by_their_shared_name() {
        let first = ArchiveName::parse("Big.Game.part01.rar").unwrap();
        assert!(first.first);
        assert_eq!(first.stem, "Big.Game");
        let third = ArchiveName::parse("big.game.part03.rar").unwrap();
        assert!(!third.first && first.same_set(&third));
        assert!(!first.same_set(&ArchiveName::parse("Other.part02.rar").unwrap()));

        let old = ArchiveName::parse("Old.rar").unwrap();
        let next = ArchiveName::parse("Old.r00").unwrap();
        assert!(old.first && !next.first && old.same_set(&next));

        let zip = ArchiveName::parse("Pics.zip").unwrap();
        let piece = ArchiveName::parse("Pics.z01").unwrap();
        assert!(zip.first && !piece.first && zip.same_set(&piece));

        let split = ArchiveName::parse("Disk.7z.001").unwrap();
        let rest = ArchiveName::parse("Disk.7z.002").unwrap();
        assert!(split.first && !rest.first && split.same_set(&rest));
        assert_eq!(split.stem, "Disk");
        // A split archive is not the unsplit one of the same name.
        assert!(!split.same_set(&ArchiveName::parse("Disk.7z").unwrap()));
    }
}
