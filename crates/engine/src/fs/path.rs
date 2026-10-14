//! Relative destination paths and the central filename policy (ADR-17).
//!
//! Adapters, imports, and clients never name an absolute path. They name a
//! [`RelativePath`] below a configured root, and every platform-specific
//! filename rule is applied here, once, so a name that would be refused or
//! silently changed by one of the supported filesystems is rejected before
//! it is reserved.

use std::fmt;

/// Longest accepted path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Longest accepted path component, in bytes. Most filesystems allow 255.
pub const MAX_COMPONENT_BYTES: usize = 255;
/// Deepest accepted path, in components.
pub const MAX_DEPTH: usize = 32;
/// First component reserved for the engine's private files in every root.
pub const PRIVATE_DIR: &str = ".wayfetch";

/// Why a path was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    /// The path is empty.
    #[error("path is empty")]
    Empty,
    /// The path or a component is too long, or the path is too deep.
    #[error("path is too long or too deep")]
    TooLong,
    /// The path is absolute or names a drive or share.
    #[error("path is not relative")]
    NotRelative,
    /// A component is empty, `.`, or `..`.
    #[error("path has an empty, `.`, or `..` component")]
    NotNormal,
    /// A component contains a character some supported filesystem refuses.
    #[error("path contains the forbidden character {0:?}")]
    ForbiddenChar(char),
    /// A component ends with a dot or space, which Windows strips.
    #[error("path component ends with a dot or space")]
    TrailingDotOrSpace,
    /// A component is a device name reserved on Windows.
    #[error("path component {0:?} is a reserved device name")]
    ReservedName(String),
    /// The path is inside the engine's private directory.
    #[error("path is inside the engine's private directory")]
    Private,
}

/// A validated, normalized path below a root: `/`-separated components,
/// none of them empty, `.`, `..`, or otherwise refused by a supported
/// filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelativePath(String);

impl RelativePath {
    /// Validates `path`. Separators are `/` only; a backslash is refused
    /// rather than reinterpreted.
    pub fn new(path: &str) -> Result<Self, PathError> {
        if path.is_empty() {
            return Err(PathError::Empty);
        }
        if path.len() > MAX_PATH_BYTES {
            return Err(PathError::TooLong);
        }
        if path.starts_with('/') {
            return Err(PathError::NotRelative);
        }
        let mut depth = 0;
        for component in path.split('/') {
            depth += 1;
            check_component(component)?;
        }
        if depth > MAX_DEPTH {
            return Err(PathError::TooLong);
        }
        if path
            .split('/')
            .next()
            .is_some_and(|first| first.eq_ignore_ascii_case(PRIVATE_DIR))
        {
            return Err(PathError::Private);
        }
        Ok(Self(path.to_owned()))
    }

    /// The path text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The parent directory, if any, and the final component.
    pub fn split_last(&self) -> (Option<&str>, &str) {
        match self.0.rsplit_once('/') {
            Some((parent, name)) => (Some(parent), name),
            None => (None, &self.0),
        }
    }

    /// The key two paths share when a case-insensitive filesystem would
    /// treat them as one name. Reservations use it, so `A.bin` and `a.bin`
    /// never both get reserved in one root.
    ///
    /// Unicode normalization forms are not folded yet: names that differ
    /// only in composed and decomposed forms reserve separately.
    pub fn collision_key(&self) -> String {
        self.0.to_lowercase()
    }

    /// The `n`th alternative name for a taken target: `dir/name (n).ext`.
    /// The sequence is deterministic, so concurrent jobs wanting one name
    /// settle on distinct names in reservation order. `n` of 0 is the path
    /// itself.
    pub fn numbered(&self, n: u32) -> Result<Self, PathError> {
        if n == 0 {
            return Ok(self.clone());
        }
        let (parent, name) = self.split_last();
        // A leading dot is part of the stem (".profile"), not an extension.
        let renamed = match name.rfind('.').filter(|&dot| dot > 0) {
            Some(dot) => format!("{} ({n}){}", &name[..dot], &name[dot..]),
            None => format!("{name} ({n})"),
        };
        match parent {
            Some(parent) => Self::new(&format!("{parent}/{renamed}")),
            None => Self::new(&renamed),
        }
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Windows device names, reserved with any extension and in any case.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn check_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() || component == "." || component == ".." {
        return Err(PathError::NotNormal);
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(PathError::TooLong);
    }
    if let Some(c) = component
        .chars()
        .find(|&c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        // A colon also rules out drive prefixes and NTFS alternate streams.
        return Err(if c == ':' && is_drive(component) {
            PathError::NotRelative
        } else {
            PathError::ForbiddenChar(c)
        });
    }
    if component.ends_with(['.', ' ']) {
        return Err(PathError::TrailingDotOrSpace);
    }
    let stem = component.split('.').next().unwrap_or(component);
    if RESERVED
        .iter()
        .any(|r| stem.trim_end().eq_ignore_ascii_case(r))
    {
        return Err(PathError::ReservedName(component.to_owned()));
    }
    Ok(())
}

fn is_drive(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_relative_paths() {
        for ok in [
            "a.bin",
            "Movies/2024/clip.mkv",
            ".hidden",
            "über/日本.zip",
            "a b/c d",
        ] {
            assert_eq!(RelativePath::new(ok).unwrap().as_str(), ok);
        }
    }

    #[test]
    fn rejects_escapes_and_platform_hazards() {
        let cases = [
            ("", PathError::Empty),
            ("/etc/passwd", PathError::NotRelative),
            ("C:/x", PathError::NotRelative),
            ("../x", PathError::NotNormal),
            ("a/../../x", PathError::NotNormal),
            ("a/./b", PathError::NotNormal),
            ("a//b", PathError::NotNormal),
            ("a/", PathError::NotNormal),
            ("a\\..\\b", PathError::ForbiddenChar('\\')),
            ("a\0b", PathError::ForbiddenChar('\0')),
            ("a\nb", PathError::ForbiddenChar('\n')),
            ("file.txt:stream", PathError::ForbiddenChar(':')),
            ("what?", PathError::ForbiddenChar('?')),
            ("name.", PathError::TrailingDotOrSpace),
            ("name ", PathError::TrailingDotOrSpace),
            ("con", PathError::ReservedName("con".into())),
            ("dir/LPT1.txt", PathError::ReservedName("LPT1.txt".into())),
            (".wayfetch/staging/x", PathError::Private),
            (".WayFetch", PathError::Private),
        ];
        for (path, expected) in cases {
            assert_eq!(RelativePath::new(path), Err(expected), "{path:?}");
        }
        assert_eq!(RelativePath::new(&"a".repeat(256)), Err(PathError::TooLong));
        assert_eq!(
            RelativePath::new(&["a"; 33].join("/")),
            Err(PathError::TooLong)
        );
        assert_eq!(
            RelativePath::new(&vec!["a".repeat(200); 6].join("/")),
            Err(PathError::TooLong)
        );
    }

    #[test]
    fn collision_keys_fold_case() {
        let a = RelativePath::new("Dir/File.BIN").unwrap();
        let b = RelativePath::new("dir/file.bin").unwrap();
        assert_ne!(a, b);
        assert_eq!(a.collision_key(), b.collision_key());
    }

    #[test]
    fn numbered_alternatives_are_deterministic() {
        let path = RelativePath::new("dir/movie.part1.rar").unwrap();
        assert_eq!(path.numbered(0).unwrap(), path);
        assert_eq!(
            path.numbered(2).unwrap().as_str(),
            "dir/movie.part1 (2).rar"
        );
        assert_eq!(
            RelativePath::new(".profile")
                .unwrap()
                .numbered(1)
                .unwrap()
                .as_str(),
            ".profile (1)"
        );
        assert_eq!(
            RelativePath::new("README")
                .unwrap()
                .numbered(3)
                .unwrap()
                .as_str(),
            "README (3)"
        );
    }
}
