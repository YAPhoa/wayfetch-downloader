//! Random secrets and the owner-only files that hold them: the API token
//! file, the browser owner account, and its setup secret.
//!
//! Secrets are compared by their SHA-256 digests, so comparison time
//! reveals nothing about a secret, only about digests an attacker cannot
//! steer. On Unix, a file or directory other users can read or write is
//! refused rather than trusted.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};
pub(crate) use wayfetch_engine::fs::{create_private, is_private};

/// Random bytes in a generated secret: 256 bits.
pub(crate) const SECRET_BYTES: usize = 32;

/// The SHA-256 digest of `secret`.
pub(crate) fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// A fresh random secret of [`SECRET_BYTES`], in hex.
pub(crate) fn generate() -> io::Result<String> {
    let mut bytes = [0; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// An owner-only file could not be read.
#[derive(Debug)]
pub(crate) enum ReadError {
    Io(io::Error),
    /// Other users may read or change it.
    Exposed,
    /// It is larger than the caller's bound.
    TooLarge,
}

/// Reads the owner-only text file at `path`, of at most `max` bytes.
pub(crate) fn read_private(path: &Path, max: u64) -> Result<String, ReadError> {
    let file = File::open(path).map_err(ReadError::Io)?;
    if !is_private(&file.metadata().map_err(ReadError::Io)?) {
        return Err(ReadError::Exposed);
    }
    let mut text = String::new();
    file.take(max + 1)
        .read_to_string(&mut text)
        .map_err(ReadError::Io)?;
    if text.len() as u64 > max {
        return Err(ReadError::TooLarge);
    }
    Ok(text)
}

/// Opens `path` for locking, creating it owner-only if missing.
#[cfg(unix)]
pub(crate) fn open_lock(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
pub(crate) fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Creates the directory `path` owner-only if it does not exist; its
/// parent must. Returns whether the existing or new directory is
/// owner-only.
pub(crate) fn private_dir(path: &Path) -> io::Result<bool> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ));
    }
    Ok(is_private(&metadata))
}

/// Removes `path`, which may already be gone.
pub(crate) fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Makes the creation, renaming, or removal of entries in `dir` durable.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_: &Path) -> io::Result<()> {
    Ok(())
}
