//! Opening, initializing, and verifying a destination root (ADR-17,
//! ADR-29).
//!
//! A destination folder is shared: any number of Wayfetch profiles (state
//! directories) may save into it at once. Each profile marks the folder
//! with its own marker, `.wayfetch/roots/<instance>.json`, naming the root
//! ID its store gave the folder, and keeps its private files under
//! `.wayfetch/staging/<instance>/`. Another profile's marker or files never
//! stop a profile from using the folder, and are never read, changed, or
//! removed by it.
//!
//! Earlier versions wrote a single marker, `.wayfetch/root.json`, and staged
//! directly in `.wayfetch/staging/`. Such a marker still identifies the
//! folder for the profile it names and is ignored by every other one; it is
//! never rewritten or removed. Staging files recorded under the old layout
//! are still reopened, published, and cleaned up by the profile that
//! recorded them, while new ones go in the profile's own folder.

use std::io;
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use wayfetch_domain::id::RootId;
use wayfetch_protocol::InstanceId;

use super::path::PRIVATE_DIR;
use super::{FileIdentity, FsError, sys};

/// Folder of per-profile markers inside [`PRIVATE_DIR`].
const MARKERS: &str = "roots";
/// The single marker earlier versions wrote inside [`PRIVATE_DIR`].
const LEGACY_MARKER: &str = "root.json";
/// Staging directory inside [`PRIVATE_DIR`]: each profile stages in its own
/// folder in it, and earlier versions staged in it directly.
pub(super) const STAGING: &str = "staging";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    instance: InstanceId,
    root: RootId,
}

/// An open, verified destination root.
#[derive(Debug)]
pub struct Root {
    pub(super) id: RootId,
    instance: InstanceId,
    /// The name of this profile's staging folder.
    pub(super) owner: String,
    path: PathBuf,
    pub(super) dir: Dir,
    identity: FileIdentity,
    /// This profile's staging folder.
    pub(super) staging: Dir,
    /// The shared staging directory, holding files earlier versions staged.
    pub(super) legacy_staging: Dir,
}

impl Root {
    /// Marks the existing directory `path` as root `id` of `instance` by
    /// writing this profile's marker and private directories. Idempotent;
    /// refuses a directory whose marker for `instance` names another root.
    /// Other profiles' markers are left alone and do not matter. This is an
    /// explicit action of the operator or user; [`Root::open`] never does
    /// it implicitly, and a missing directory is never created.
    pub fn initialize(path: &Path, id: RootId, instance: InstanceId) -> Result<(), FsError> {
        let dir = open_root_dir(path)?;
        match read_marker(&dir, instance)? {
            Some(found) if found == id => {}
            Some(_) => return Err(FsError::WrongRoot(id)),
            None => {
                create_dir(&dir, PRIVATE_DIR)?;
                let private = dir
                    .open_dir(PRIVATE_DIR)
                    .map_err(FsError::io("open private directory"))?;
                create_dir(&private, MARKERS)?;
                let markers = private
                    .open_dir(MARKERS)
                    .map_err(FsError::io("open marker directory"))?;
                let json =
                    serde_json::to_vec(&Marker { instance, root: id }).expect("marker serializes");
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                let mut file = markers
                    .open_with(marker_name(instance), &options)
                    .map_err(FsError::io("create folder marker"))?;
                io::Write::write_all(&mut file, &json)
                    .and_then(|()| file.sync_all())
                    .map_err(FsError::io("write folder marker"))?;
                sys::sync_dir(&markers).map_err(FsError::io("sync marker directory"))?;
            }
        }
        let private = dir
            .open_dir(PRIVATE_DIR)
            .map_err(FsError::io("open private directory"))?;
        create_dir(&private, STAGING)?;
        let staging = private
            .open_dir(STAGING)
            .map_err(FsError::io("open staging directory"))?;
        create_dir(&staging, &instance.to_string())
    }

    /// Opens root `id` at `path`, failing closed unless this profile's
    /// marker names this root. Creates nothing in a directory that fails
    /// the check; in one that passes, creates this profile's staging folder
    /// if a marker of an earlier version is all it has.
    pub fn open(path: &Path, id: RootId, instance: InstanceId) -> Result<Self, FsError> {
        let dir = open_root_dir(path)?;
        match read_marker(&dir, instance)? {
            Some(found) if found == id => {}
            Some(_) => return Err(FsError::WrongRoot(id)),
            None => return Err(FsError::MarkerMissing(id)),
        }
        let identity = sys::identity(&dir.dir_metadata().map_err(FsError::io("inspect root"))?)?;
        let legacy_staging = dir
            .open_dir(Path::new(PRIVATE_DIR).join(STAGING))
            .map_err(FsError::io("open staging directory"))?;
        let owner = instance.to_string();
        create_dir(&legacy_staging, &owner)?;
        let staging = legacy_staging
            .open_dir(&owner)
            .map_err(FsError::io("open staging directory"))?;
        Ok(Self {
            id,
            instance,
            owner,
            path: path.to_owned(),
            dir,
            identity,
            staging,
            legacy_staging,
        })
    }

    /// The root the directory at `path` is marked as for `instance`, or
    /// `None` if it carries no marker of this profile (never initialized by
    /// it, used only by other profiles, or an empty mount point). Fails if
    /// the directory is unavailable or this profile's marker is unreadable.
    /// Creates nothing.
    pub fn identify(path: &Path, instance: InstanceId) -> Result<Option<RootId>, FsError> {
        read_marker(&open_root_dir(path)?, instance)
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
    /// and still carries this profile's marker. A held handle keeps working
    /// on a detached mount, so this runs before anything becomes visible to
    /// the user.
    pub fn verify(&self) -> Result<(), FsError> {
        let current = Self::open(&self.path, self.id, self.instance)?;
        if !current.identity.matches(&self.identity) {
            return Err(FsError::RootReplaced(self.id));
        }
        Ok(())
    }
}

/// The file name of `instance`'s marker inside [`MARKERS`].
fn marker_name(instance: InstanceId) -> String {
    format!("{instance}.json")
}

fn open_root_dir(path: &Path) -> Result<Dir, FsError> {
    // A hung mount, for tests: configured to pause, opening a folder named
    // `stalled` blocks until the failpoint is removed.
    #[cfg(feature = "failpoints")]
    fail::fail_point!(
        "root-open-stall",
        path.file_name() == Some(std::ffi::OsStr::new("stalled")),
        |_| Err(FsError::io(super::OPEN_ROOT)(io::Error::other("failpoint")))
    );
    Dir::open_ambient_dir(path, ambient_authority()).map_err(FsError::io(super::OPEN_ROOT))
}

fn create_dir(parent: &Dir, name: &str) -> Result<(), FsError> {
    match parent.create_dir(name) {
        Ok(()) => sys::sync_dir(parent).map_err(FsError::io("sync directory")),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(FsError::io("create directory")(e)),
    }
}

/// The root `instance`'s marker in `dir` names, `None` if it has none. Its
/// own marker is read first, then the single marker of an earlier version,
/// which counts only if it names `instance`: another profile's, or one that
/// does not parse and so names nobody, is ignored.
fn read_marker(dir: &Dir, instance: InstanceId) -> Result<Option<RootId>, FsError> {
    let read = |path: PathBuf| match dir.read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(FsError::io("read folder marker")(e)),
    };
    let private = Path::new(PRIVATE_DIR);
    if let Some(bytes) = read(private.join(MARKERS).join(marker_name(instance)))? {
        return match serde_json::from_slice::<Marker>(&bytes) {
            Ok(found) if found.instance == instance => Ok(Some(found.root)),
            _ => Err(FsError::UnreadableMarker),
        };
    }
    Ok(read(private.join(LEGACY_MARKER))?
        .and_then(|bytes| serde_json::from_slice::<Marker>(&bytes).ok())
        .filter(|found| found.instance == instance)
        .map(|found| found.root))
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
            Err(FsError::MarkerMissing(_))
        ));
        // Failing closed wrote nothing into what may be a bare mount point.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);

        Root::initialize(tmp.path(), id, instance).unwrap();
        Root::initialize(tmp.path(), id, instance).unwrap();
        let root = Root::open(tmp.path(), id, instance).unwrap();
        assert_eq!(root.id(), id);
        root.verify().unwrap();
    }

    fn marker(tmp: &Path, instance: InstanceId) -> PathBuf {
        tmp.join(PRIVATE_DIR)
            .join(MARKERS)
            .join(marker_name(instance))
    }

    fn write_legacy(tmp: &Path, id: RootId, instance: InstanceId) {
        std::fs::create_dir_all(tmp.join(PRIVATE_DIR).join(STAGING)).unwrap();
        std::fs::write(
            tmp.join(PRIVATE_DIR).join(LEGACY_MARKER),
            serde_json::to_vec(&Marker { instance, root: id }).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn refuses_a_marker_naming_another_root_of_the_same_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        Root::initialize(tmp.path(), id, instance).unwrap();
        let other = RootId::from_u64(2).unwrap();
        assert!(matches!(
            Root::open(tmp.path(), other, instance),
            Err(FsError::WrongRoot(_))
        ));
        assert!(matches!(
            Root::initialize(tmp.path(), other, instance),
            Err(FsError::WrongRoot(_))
        ));
        std::fs::write(marker(tmp.path(), instance), b"garbage").unwrap();
        assert!(matches!(
            Root::open(tmp.path(), id, instance),
            Err(FsError::UnreadableMarker)
        ));
    }

    #[test]
    fn profiles_share_a_folder_with_their_own_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, a) = ids();
        let b = InstanceId::generate();
        Root::initialize(tmp.path(), id, a).unwrap();
        // Another profile's marker is not this profile's: the folder is
        // not set up for it yet, and setting it up leaves the first alone.
        assert_eq!(Root::identify(tmp.path(), b).unwrap(), None);
        assert!(matches!(
            Root::open(tmp.path(), id, b),
            Err(FsError::MarkerMissing(_))
        ));
        let first = std::fs::read(marker(tmp.path(), a)).unwrap();
        // The second profile's store may give the folder any ID, even one
        // the first uses for it.
        let other = RootId::from_u64(9).unwrap();
        Root::initialize(tmp.path(), other, b).unwrap();
        assert_eq!(std::fs::read(marker(tmp.path(), a)).unwrap(), first);
        assert_eq!(Root::identify(tmp.path(), a).unwrap(), Some(id));
        assert_eq!(Root::identify(tmp.path(), b).unwrap(), Some(other));
        let (ra, rb) = (
            Root::open(tmp.path(), id, a).unwrap(),
            Root::open(tmp.path(), other, b).unwrap(),
        );
        ra.verify().unwrap();
        rb.verify().unwrap();
        // Each stages in its own folder.
        let staging = tmp.path().join(PRIVATE_DIR).join(STAGING);
        assert!(staging.join(a.to_string()).is_dir());
        assert!(staging.join(b.to_string()).is_dir());
        // Another profile's unreadable marker is not this one's problem.
        std::fs::write(marker(tmp.path(), b), b"garbage").unwrap();
        ra.verify().unwrap();
    }

    #[test]
    fn an_earlier_versions_marker_identifies_only_its_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        write_legacy(tmp.path(), id, instance);
        let legacy = std::fs::read(tmp.path().join(PRIVATE_DIR).join(LEGACY_MARKER)).unwrap();
        assert_eq!(Root::identify(tmp.path(), instance).unwrap(), Some(id));
        let root = Root::open(tmp.path(), id, instance).unwrap();
        root.verify().unwrap();
        Root::initialize(tmp.path(), id, instance).unwrap();
        // Another profile may use the folder; the old marker stays as it was.
        let other = InstanceId::generate();
        assert_eq!(Root::identify(tmp.path(), other).unwrap(), None);
        Root::initialize(tmp.path(), RootId::from_u64(3).unwrap(), other).unwrap();
        Root::open(tmp.path(), RootId::from_u64(3).unwrap(), other).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join(PRIVATE_DIR).join(LEGACY_MARKER)).unwrap(),
            legacy
        );
        root.verify().unwrap();
        // An old marker that does not parse names nobody.
        std::fs::write(tmp.path().join(PRIVATE_DIR).join(LEGACY_MARKER), b"garbage").unwrap();
        assert_eq!(Root::identify(tmp.path(), instance).unwrap(), None);
    }

    #[test]
    fn identifies_only_this_profiles_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let (id, instance) = ids();
        assert_eq!(Root::identify(tmp.path(), instance).unwrap(), None);
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        Root::initialize(tmp.path(), id, instance).unwrap();
        assert_eq!(Root::identify(tmp.path(), instance).unwrap(), Some(id));
        assert_eq!(
            Root::identify(tmp.path(), InstanceId::generate()).unwrap(),
            None
        );
        std::fs::write(marker(tmp.path(), instance), b"garbage").unwrap();
        assert!(matches!(
            Root::identify(tmp.path(), instance),
            Err(FsError::UnreadableMarker)
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

        // The mount disappears: the path is now an empty directory, which
        // is never set up again behind the user's back.
        std::fs::rename(&path, tmp.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(root.verify(), Err(FsError::MarkerMissing(_))));
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);

        // A copy of the marker in a different directory is not the root.
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
