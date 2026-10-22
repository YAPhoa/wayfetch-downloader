//! Reading checksum files: SFV, and the formats of GNU `md5sum`, `sha1sum`,
//! and `sha256sum`, untagged or with BSD-style tags.
//!
//! The subset read: a UTF-8 BOM is skipped; lines end in LF or CRLF; a line
//! that is not UTF-8, or not in the file's format, is skipped and counted;
//! names are relative, use `/` between folders, and never contain `..`.
//! SFV: `;` starts a comment, the value is the 8 hex digits after the last
//! run of spaces or tabs, and `\` separates folders. GNU: `#` starts a
//! comment, `HEX  NAME` or `HEX *NAME`, or `TAG (NAME) = HEX`; a line
//! starting with `\` has an escaped name (`\\`, `\n`, `\r`).

use super::{Algorithm, Digest};

/// Largest checksum file read.
pub const MAX_BYTES: u64 = 1 << 20;

/// Most entries read from one checksum file.
pub const MAX_ENTRIES: usize = 10_000;

/// A checksum file's format, known from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An SFV file: CRC-32 values.
    Sfv,
    /// A GNU checksum file of the algorithm its name gives.
    Gnu(Algorithm),
}

impl Kind {
    /// The format of a file named `name`, if it is a checksum file:
    /// `.sfv`, `.md5`, `.sha1`, `.sha256`, or `MD5SUMS`, `SHA1SUMS`,
    /// `SHA256SUMS`, in any case.
    pub fn of(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        let (stem, extension) = lower.rsplit_once('.').unwrap_or(("", &lower));
        match (stem.is_empty(), extension) {
            (false, "sfv") => Some(Self::Sfv),
            (false, "md5") | (true, "md5sums") => Some(Self::Gnu(Algorithm::Md5)),
            (false, "sha1") | (true, "sha1sums") => Some(Self::Gnu(Algorithm::Sha1)),
            (false, "sha256") | (true, "sha256sums") => Some(Self::Gnu(Algorithm::Sha256)),
            _ => None,
        }
    }
}

/// One value a checksum file lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file it is for, relative to the checksum file's folder.
    pub name: String,
    /// Its value.
    pub digest: Digest,
}

/// What a checksum file lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Its entries, in order.
    pub entries: Vec<Entry>,
    /// Lines that were not comments, blank, or entries.
    pub skipped: usize,
}

/// Why a checksum file was not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    /// It is larger than [`MAX_BYTES`].
    #[error("the checksum file is larger than 1 MiB")]
    TooLarge,
    /// It lists more than [`MAX_ENTRIES`] entries.
    #[error("the checksum file lists more than 10,000 files")]
    TooManyEntries,
}

/// Reads a checksum file of format `kind`.
pub fn parse(kind: Kind, bytes: &[u8]) -> Result<Manifest, ManifestError> {
    if bytes.len() as u64 > MAX_BYTES {
        return Err(ManifestError::TooLarge);
    }
    let bytes = bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(bytes);
    let mut manifest = Manifest::default();
    for line in bytes.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Ok(line) = std::str::from_utf8(line) else {
            manifest.skipped += 1;
            continue;
        };
        let read = match kind {
            Kind::Sfv => sfv_line(line),
            Kind::Gnu(algorithm) => gnu_line(algorithm, line),
        };
        match read {
            Line::Blank => {}
            Line::Invalid => manifest.skipped += 1,
            Line::Entry(entry) => {
                if manifest.entries.len() == MAX_ENTRIES {
                    return Err(ManifestError::TooManyEntries);
                }
                manifest.entries.push(entry);
            }
        }
    }
    Ok(manifest)
}

enum Line {
    Blank,
    Invalid,
    Entry(Entry),
}

fn entry(name: &str, digest: Option<Digest>) -> Line {
    match (relative(name), digest) {
        (Some(name), Some(digest)) => Line::Entry(Entry { name, digest }),
        _ => Line::Invalid,
    }
}

fn sfv_line(line: &str) -> Line {
    let line = line.trim_end_matches([' ', '\t']);
    if line.trim_start().is_empty() || line.starts_with(';') {
        return Line::Blank;
    }
    let Some((name, value)) = line.rsplit_once([' ', '\t']) else {
        return Line::Invalid;
    };
    let name = name.trim_matches([' ', '\t']).replace('\\', "/");
    entry(&name, Digest::parse(Algorithm::Crc32, value))
}

fn gnu_line(algorithm: Algorithm, line: &str) -> Line {
    if line.trim().is_empty() || line.starts_with('#') {
        return Line::Blank;
    }
    let (escaped, line) = match line.strip_prefix('\\') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (raw, digest) = match tagged(line) {
        Some((tag, name, value)) => (name, tag.and_then(|tag| Digest::parse(tag, value))),
        None => {
            let Some((value, rest)) = line.split_once(' ') else {
                return Line::Invalid;
            };
            let Some(name) = rest.strip_prefix([' ', '*']) else {
                return Line::Invalid;
            };
            (name, Digest::parse(algorithm, value))
        }
    };
    let name = if escaped {
        match unescape(raw) {
            Some(name) => name,
            None => return Line::Invalid,
        }
    } else {
        raw.to_owned()
    };
    entry(&name, digest)
}

/// A BSD-style tagged line, `TAG (NAME) = HEX`: its algorithm if one of
/// those read, the name, and the value.
fn tagged(line: &str) -> Option<(Option<Algorithm>, &str, &str)> {
    let (tag, rest) = line.split_once(" (")?;
    let (name, value) = rest.rsplit_once(") = ")?;
    let algorithm = match tag {
        "MD5" => Some(Algorithm::Md5),
        "SHA1" => Some(Algorithm::Sha1),
        "SHA256" | "SHA2-256" => Some(Algorithm::Sha256),
        "CRC32" => Some(Algorithm::Crc32),
        _ if tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => None,
        _ => return None,
    };
    Some((algorithm, name, value))
}

/// An escaped GNU name: `\\`, `\n`, and `\r`; any other escape is invalid.
fn unescape(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// `name` as a relative path below the checksum file's folder, without
/// leading `./`: `None` if it is empty, absolute, or leaves the folder.
fn relative(name: &str) -> Option<String> {
    let mut name = name;
    while let Some(rest) = name.strip_prefix("./") {
        name = rest;
    }
    if name.is_empty() || name.starts_with('/') {
        return None;
    }
    let ok = name
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..");
    ok.then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(manifest: &Manifest) -> Vec<(&str, String)> {
        manifest
            .entries
            .iter()
            .map(|e| (e.name.as_str(), e.digest.to_string()))
            .collect()
    }

    #[test]
    fn formats_are_known_by_name() {
        assert_eq!(Kind::of("Release.SFV"), Some(Kind::Sfv));
        assert_eq!(Kind::of("a.md5"), Some(Kind::Gnu(Algorithm::Md5)));
        assert_eq!(Kind::of("SHA256SUMS"), Some(Kind::Gnu(Algorithm::Sha256)));
        assert_eq!(Kind::of("x.sha1"), Some(Kind::Gnu(Algorithm::Sha1)));
        assert_eq!(Kind::of(".sfv"), None);
        assert_eq!(Kind::of("sha256sums.txt"), None);
        assert_eq!(Kind::of("movie.mkv"), None);
    }

    #[test]
    fn sfv_lines() {
        let file = "\u{feff}; made by a tool\r\n\
                    show.part1.rar 0A1B2C3D\r\n\
                    my file.r00\t\tdeadbeef\n\
                    sub\\inner.bin 00000000\n\
                    \n\
                    bad line\n\
                    short 1234567\n\
                    ../escape.bin 00000001\n";
        let manifest = parse(Kind::Sfv, file.as_bytes()).unwrap();
        assert_eq!(
            names(&manifest),
            [
                ("show.part1.rar", "crc32:0a1b2c3d".into()),
                ("my file.r00", "crc32:deadbeef".into()),
                ("sub/inner.bin", "crc32:00000000".into()),
            ]
        );
        assert_eq!(manifest.skipped, 3);
    }

    #[test]
    fn gnu_lines() {
        let md5 = "d41d8cd98f00b204e9800998ecf8427e";
        let file = format!(
            "# comment\n\
             {md5}  ./plain name.txt\n\
             {md5} *binary.bin\n\
             \\{md5}  new\\nline\\\\x\n\
             MD5 (tagged (1).txt) = {md5}\n\
             SHA256 (wrong tag.txt) = {md5}\n\
             {md5} one-space.txt\n\
             \\{md5}  bad\\tescape\n\
             {md5}  /etc/passwd\n"
        );
        let manifest = parse(Kind::Gnu(Algorithm::Md5), file.as_bytes()).unwrap();
        let digest = format!("md5:{md5}");
        assert_eq!(
            names(&manifest),
            [
                ("plain name.txt", digest.clone()),
                ("binary.bin", digest.clone()),
                ("new\nline\\x", digest.clone()),
                ("tagged (1).txt", digest),
            ]
        );
        assert_eq!(manifest.skipped, 4);
    }

    #[test]
    fn tagged_lines_may_name_another_algorithm() {
        let sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let manifest = parse(
            Kind::Gnu(Algorithm::Sha256),
            format!("SHA1 (a) = {sha1}\nBLAKE2b (b) = 00\n").as_bytes(),
        )
        .unwrap();
        assert_eq!(names(&manifest), [("a", format!("sha1:{sha1}"))]);
        assert_eq!(manifest.skipped, 1);
    }

    #[test]
    fn lines_that_are_not_utf8_are_skipped() {
        let mut file = b"a.bin 00000001\n".to_vec();
        file.extend_from_slice(b"\xff\xfe.bin 00000002\n");
        let manifest = parse(Kind::Sfv, &file).unwrap();
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.skipped, 1);
    }

    #[test]
    fn size_and_entries_are_bounded() {
        let big = vec![b';'; MAX_BYTES as usize + 1];
        assert_eq!(parse(Kind::Sfv, &big), Err(ManifestError::TooLarge));
        let many = "f 00000000\n".repeat(MAX_ENTRIES + 1);
        assert_eq!(
            parse(Kind::Sfv, many.as_bytes()),
            Err(ManifestError::TooManyEntries)
        );
    }
}
