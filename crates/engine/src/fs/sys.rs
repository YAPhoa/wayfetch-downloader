//! The few platform-specific primitives the filesystem protocol needs.

use std::fs::File;
use std::io;

use cap_std::fs::{Dir, Metadata};

use super::{FileIdentity, FsError};

/// Identity of the file `meta` describes.
#[cfg(unix)]
pub(super) fn identity(meta: &Metadata) -> Result<FileIdentity, FsError> {
    use cap_std::fs::MetadataExt;
    let birth_ns = meta
        .created()
        .ok()
        .and_then(|t| t.into_std().duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_nanos()).ok());
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
        birth_ns,
    })
}

#[cfg(not(unix))]
pub(super) fn identity(_meta: &Metadata) -> Result<FileIdentity, FsError> {
    Err(FsError::Unsupported("file identity"))
}

/// Makes `dir`'s entries durable: new, linked, and removed names.
#[cfg(unix)]
pub(super) fn sync_dir(dir: &Dir) -> io::Result<()> {
    // `Dir` handles may be path-only descriptors, which cannot be synced;
    // a read-only handle to the same directory can.
    dir.open_with(".", cap_std::fs::OpenOptions::new().read(true))?
        .sync_all()
}

/// Windows has no portable directory flush; NTFS journals metadata.
#[cfg(not(unix))]
pub(super) fn sync_dir(_dir: &Dir) -> io::Result<()> {
    Ok(())
}

/// Writes all of `buf` at `offset` without moving a shared cursor, so
/// concurrent writers of disjoint intervals need no lock.
#[cfg(unix)]
pub(super) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
pub(super) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => {
                buf = &buf[n..];
                offset += n as u64;
            }
        }
    }
    Ok(())
}
