//! Exclusive state-directory ownership and instance identity (ADR-34,
//! test T26).
//!
//! An engine holds an exclusive advisory lock on `<state_dir>/wayfetch.lock`
//! for as long as any of its tasks may touch the directory. A second engine,
//! in this process or another, fails to start instead of waiting or taking
//! over. The lock is released by the operating system when the holder's file
//! handle closes, including on a crash, so no stale-lock cleanup exists or is
//! needed.
//!
//! `<state_dir>/instance.json` records the instance identity and the last
//! boot epoch. Each start advances the epoch and replaces the file
//! atomically (temporary file, fsync, rename, directory fsync).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use wayfetch_domain::version::BootEpoch;
use wayfetch_protocol::InstanceId;

const LOCK_FILE: &str = "wayfetch.lock";
const IDENTITY_FILE: &str = "instance.json";

/// Why the state directory could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum StateDirError {
    /// Another engine owns the directory.
    #[error("state directory {0} is owned by another running engine")]
    Locked(PathBuf),
    /// The directory or its identity file could not be used.
    #[error("state directory {path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The identity file is not valid.
    #[error("identity file {path} is corrupt: {source}")]
    CorruptIdentity {
        /// The path involved.
        path: PathBuf,
        /// The parse error.
        #[source]
        source: serde_json::Error,
    },
    /// The boot epoch cannot advance further.
    #[error("boot epoch exhausted")]
    EpochExhausted,
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> StateDirError + '_ {
    move |source| StateDirError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Stored identity of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Stable instance identity.
    pub instance: InstanceId,
    /// Epoch of the current (or last) engine start.
    pub boot_epoch: BootEpoch,
}

/// An exclusively locked state directory. Dropping it releases the lock.
#[derive(Debug)]
pub struct StateDir {
    path: PathBuf,
    identity: Identity,
    _lock: File,
}

impl StateDir {
    /// Locks `path`, creating its final component if needed, and records a
    /// new boot epoch. Blocking; call from a blocking context.
    pub fn open(path: &Path) -> Result<Self, StateDirError> {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(io_err(path)(e)),
        }
        let lock_path = path.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_err(&lock_path))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(StateDirError::Locked(path.to_owned())),
            Err(TryLockError::Error(e)) => return Err(io_err(&lock_path)(e)),
        }
        let identity = advance_identity(path)?;
        Ok(Self {
            path: path.to_owned(),
            identity,
            _lock: lock,
        })
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The identity recorded for this start.
    pub fn identity(&self) -> Identity {
        self.identity
    }
}

fn advance_identity(dir: &Path) -> Result<Identity, StateDirError> {
    let path = dir.join(IDENTITY_FILE);
    let identity = match fs::read(&path) {
        Ok(bytes) => {
            let stored: Identity = serde_json::from_slice(&bytes).map_err(|source| {
                StateDirError::CorruptIdentity {
                    path: path.clone(),
                    source,
                }
            })?;
            Identity {
                instance: stored.instance,
                boot_epoch: stored
                    .boot_epoch
                    .next()
                    .map_err(|_| StateDirError::EpochExhausted)?,
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Identity {
            instance: InstanceId::generate(),
            boot_epoch: BootEpoch::INITIAL,
        },
        Err(e) => return Err(io_err(&path)(e)),
    };
    let bytes = serde_json::to_vec(&identity).map_err(io::Error::from);
    bytes
        .and_then(|bytes| replace_file(&path, &bytes))
        .map_err(io_err(&path))?;
    Ok(identity)
}

/// Replaces the file at `path` with `bytes` atomically and durably: a
/// temporary file in the same directory, synced, renamed over it, and the
/// directory synced. A crash leaves the old content or the new, never a
/// mix.
pub fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_owner_is_refused_until_the_first_releases() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let first = StateDir::open(&state).unwrap();
        assert!(matches!(
            StateDir::open(&state),
            Err(StateDirError::Locked(_))
        ));
        let id = first.identity();
        drop(first);
        let second = StateDir::open(&state).unwrap();
        assert_eq!(second.identity().instance, id.instance);
        assert_eq!(second.identity().boot_epoch, id.boot_epoch.next().unwrap());
    }

    #[test]
    fn missing_parent_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("unmounted").join("state");
        assert!(matches!(
            StateDir::open(&state),
            Err(StateDirError::Io { .. })
        ));
        assert!(!dir.path().join("unmounted").exists());
    }

    #[test]
    fn corrupt_identity_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(IDENTITY_FILE), b"not json").unwrap();
        assert!(matches!(
            StateDir::open(dir.path()),
            Err(StateDirError::CorruptIdentity { .. })
        ));
    }

    #[test]
    fn a_file_is_not_a_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, b"").unwrap();
        assert!(StateDir::open(&file).is_err());
    }
}
