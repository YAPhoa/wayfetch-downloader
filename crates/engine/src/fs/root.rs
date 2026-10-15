//! Opening, initializing, and verifying a destination root (ADR-17,
//! ADR-29).

use std::io;
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use wayfetch_domain::id::RootId;
use wayfetch_protocol::InstanceId;

use super::path::PRIVATE_DIR;
use super::{FileIdentity, FsError, sys};

/// Sentinel file inside [`PRIVATE_DIR`].
const SENTINEL: &str = "root.json";
/// Staging directory inside [`PRIVATE_DIR`].
const STAGING: &str = "staging";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Sentinel {
    instance: InstanceId,
    root: RootId,
}

/// An open, verified destination root.
#[derive(Debug)]
pub struct Root {
    pub(super) id: RootId,
    instance: InstanceId,
    path: PathBuf,
    pub(super) dir: Dir,
    identity: FileIdentity,
    pub(super) staging: Dir,
}

impl Root {
    /// Marks the existing directory `path` as root `id` of `instance` by
    /// writing its sentinel and private directories. Idempotent; refuses a
    /// directory already marked for another root or instance. This is an
    /// explicit operator action; [`Root::open`] never does it implicitly.
    pub fn initialize(path: &Path, id: RootId, instance: InstanceId) -> Result<(), FsError> {
        let dir = open_root_dir(path)?;
        let expected = Sentinel { instance, root: id };
        match read_sentinel(&dir, || FsError::WrongRoot(id))? {
            Some(found) if found == expected => {}
            Some(_) => return Err(FsError::WrongRoot(id)),
            None => {
                create_dir(&dir, PRIVATE_DIR)?;
                let private = dir
                    .open_dir(PRIVATE_DIR)
                    .map_err(FsError::io("open private directory"))?;
                let json = serde_json::to_vec(&expected).expect("sentinel serializes");
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                let mut file = private
                    .open_with(SENTINEL, &options)
                    .map_err(FsError::io("create root sentinel"))?;
                io::Write::write_all(&mut file, &json)
                    .and_then(|()| file.sync_all())
                    .map_err(FsError::io("write root sentinel"))?;
                sys::sync_dir(&private).map_err(FsError::io("sync private directory"))?;
            }
        }
        let private = dir
            .open_dir(PRIVATE_DIR)
            .map_err(FsError::io("open private directory"))?;
        create_dir(&private, STAGING)
    }

    /// Opens root `id` at `path`, failing closed unless its sentinel names
    /// this root and instance. Creates nothing in a directory that fails the
    /// check.
    pub fn open(path: &Path, id: RootId, instance: InstanceId) -> Result<Self, FsError> {
        let dir = open_root_dir(path)?;
        match read_sentinel(&dir, || FsError::WrongRoot(id))? {
            Some(found) if found == (Sentinel { instance, root: id }) => {}
            Some(_) => return Err(FsError::WrongRoot(id)),
            None => return Err(FsError::SentinelMissing(id)),
        }
        let identity = sys::identity(&dir.dir_metadata().map_err(FsError::io("inspect root"))?)?;
        let staging = dir
            .open_dir(Path::new(PRIVATE_DIR).join(STAGING))
            .map_err(FsError::io("open staging directory"))?;
        Ok(Self {
            id,
            instance,
            path: path.to_owned(),
            dir,
            identity,
            staging,
        })
    }

    /// The root the directory at `path` is marked as for `instance`, or
    /// `None` if it carries no sentinel (never initialized, or an empty
    /// mount point). Fails if the directory is unavailable or carries an
    /// unreadable sentinel or another instance's. Creates nothing.
    pub fn identify(path: &Path, instance: InstanceId) -> Result<Option<RootId>, FsError> {
        let dir = open_root_dir(path)?;
        match read_sentinel(&dir, || FsError::ForeignSentinel)? {
            Some(found) if found.instance == instance => Ok(Some(found.root)),
            Some(_) => Err(FsError::ForeignSentinel),
            None => Ok(None),
        }
    }

    /// The root's ID.
    pub fn id(&self) -> RootId {
        self.id
    }

    /// Where the root was opened.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Checks by path that the root is still the directory that was opened
    /// and still carries its sentinel. A held handle keeps working on a
    /// detached mount, so this runs before anything becomes visible to the
    /// user.
    pub fn verify(&self) -> Result<(), FsError> {
        let current = Self::open(&self.path, self.id, self.instance)?;
        if !current.identity.matches(&self.identity) {
            return Err(FsError::RootReplaced(self.id));
        }
        Ok(())
    }
}

fn open_root_dir(path: &Path) -> Result<Dir, FsError> {
    Dir::open_ambient_dir(path, ambient_authority()).map_err(FsError::io("open root"))
}

fn create_dir(parent: &Dir, name: &str) -> Result<(), FsError> {
    match parent.create_dir(name) {
        Ok(()) => sys::sync_dir(parent).map_err(FsError::io("sync directory")),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(FsError::io("create directory")(e)),
    }
}

/// The sentinel, `None` if absent, or `unreadable()` if it does not parse.
fn read_sentinel(
    dir: &Dir,
    unreadable: impl FnOnce() -> FsError,
) -> Result<Option<Sentinel>, FsError> {
    let bytes = match dir.read(Path::new(PRIVATE_DIR).join(SENTINEL)) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(FsError::io("read root sentinel")(e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| unreadable())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (RootId, InstanceId) {
        (RootId::from_u64(1).unwrap(), InstanceId::generate())
    }

    #[test]
    fn opens_only_an_initialized_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        assert!(matches!(
            Root::open(tmp.path(), id, instance),
            Err(FsError::SentinelMissing(_))
        ));
        // Failing closed wrote nothing into what may be a bare mount point.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);

        Root::initialize(tmp.path(), id, instance).unwrap();
        Root::initialize(tmp.path(), id, instance).unwrap();
        let root = Root::open(tmp.path(), id, instance).unwrap();
        assert_eq!(root.id(), id);
        root.verify().unwrap();
    }

    #[test]
    fn refuses_another_roots_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        Root::initialize(tmp.path(), id, instance).unwrap();
        let other = RootId::from_u64(2).unwrap();
        assert!(matches!(
            Root::open(tmp.path(), other, instance),
            Err(FsError::WrongRoot(_))
        ));
        assert!(matches!(
            Root::open(tmp.path(), id, InstanceId::generate()),
            Err(FsError::WrongRoot(_))
        ));
        assert!(matches!(
            Root::initialize(tmp.path(), other, instance),
            Err(FsError::WrongRoot(_))
        ));
        std::fs::write(tmp.path().join(PRIVATE_DIR).join(SENTINEL), b"garbage").unwrap();
        assert!(matches!(
            Root::open(tmp.path(), id, instance),
            Err(FsError::WrongRoot(_))
        ));
    }

    #[test]
    fn identifies_only_this_instances_sentinel() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        assert_eq!(Root::identify(tmp.path(), instance).unwrap(), None);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        Root::initialize(tmp.path(), id, instance).unwrap();
        assert_eq!(Root::identify(tmp.path(), instance).unwrap(), Some(id));
        assert!(matches!(
            Root::identify(tmp.path(), InstanceId::generate()),
            Err(FsError::ForeignSentinel)
        ));
        std::fs::write(tmp.path().join(PRIVATE_DIR).join(SENTINEL), b"garbage").unwrap();
        assert!(matches!(
            Root::identify(tmp.path(), instance),
            Err(FsError::ForeignSentinel)
        ));
        assert!(Root::identify(&tmp.path().join("absent"), instance).is_err());
    }

    #[test]
    fn a_missing_root_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        let err = Root::open(&tmp.path().join("absent"), id, instance).unwrap_err();
        assert_eq!(
            err.failure_code(),
            wayfetch_domain::failure::FailureCode::DestinationUnavailable
        );
    }

    #[test]
    fn verify_notices_a_replaced_root() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("media");
        std::fs::create_dir(&path).unwrap();
        let (id, instance) = ids();
        Root::initialize(&path, id, instance).unwrap();
        let root = Root::open(&path, id, instance).unwrap();

        // The mount disappears: the path is now an empty directory.
        std::fs::rename(&path, tmp.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(root.verify(), Err(FsError::SentinelMissing(_))));

        // A copy of the sentinel in a different directory is not the root.
        std::fs::remove_dir(&path).unwrap();
        copy_tree(&tmp.path().join("old"), &path);
        assert!(matches!(root.verify(), Err(FsError::RootReplaced(_))));
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}
