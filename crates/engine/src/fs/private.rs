//! Owner-only files, such as the credential key and the API's token and
//! owner files.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

/// Creates `path`, which must not exist, readable and writable only by
/// its owner.
pub fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Whether only the owner may read or change what `metadata` describes.
/// Other platforms' permissions are not checked.
#[cfg(unix)]
pub fn is_private(metadata: &Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 == 0
}

#[cfg(not(unix))]
pub fn is_private(_: &Metadata) -> bool {
    true
}
