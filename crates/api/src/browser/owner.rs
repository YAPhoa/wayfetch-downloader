//! The local owner account and the setup secret that creates it
//! (architecture brief section 8.6).
//!
//! Both live in the operator's auth directory, owner-only:
//!
//! - `setup-secret` holds a random single-use secret, written at startup
//!   while no owner exists and valid for [`SETUP_LIFETIME`]. Only its
//!   digest is kept in memory, and it is never printed.
//! - `owner-account` holds the owner's name and an Argon2id hash of their
//!   password as a PHC string. It is written to a temporary file and then
//!   hard-linked into place, which fails if an owner already exists, so
//!   two setups, even by two processes, can never both succeed or leave a
//!   partial account behind.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};
use wayfetch_engine::protocol::ErrorCode;

use super::BrowserAuthError;
use crate::error::ApiError;
use crate::secret::{self, ReadError};

/// The owner account file.
pub(super) const OWNER_FILE: &str = "owner-account";
/// Where a new owner account is written before it is linked into place.
const OWNER_TEMP: &str = "owner-account.tmp";
/// The setup secret file.
pub(super) const SETUP_FILE: &str = "setup-secret";
/// How long a setup secret is accepted.
pub(super) const SETUP_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Largest owner account file accepted.
const MAX_OWNER_FILE_BYTES: u64 = 16 * 1024;
/// Shortest accepted password, in characters.
pub(super) const MIN_PASSWORD_CHARS: usize = 12;
/// Longest accepted password, in bytes; bounds the work of hashing it.
pub(super) const MAX_PASSWORD_BYTES: usize = 1024;
/// Longest accepted owner name, in characters.
const MAX_USERNAME_CHARS: usize = 64;
/// The account file's format.
const VERSION: u32 = 1;

/// The owner account.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Owner {
    version: u32,
    pub username: String,
    /// Argon2id, as a PHC string with its own salt and parameters.
    password_hash: String,
}

impl Owner {
    /// An account for `username` with `password`, hashed with Argon2id's
    /// recommended parameters. Blocks for the hashing.
    pub(super) fn new(username: String, password: &str) -> io::Result<Self> {
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes())
            .map_err(io::Error::other)?
            .to_string();
        Ok(Self {
            version: VERSION,
            username,
            password_hash,
        })
    }

    /// Whether `username` and `password` are this owner's. Always hashes
    /// the password, so a wrong name takes as long as a wrong password.
    /// Blocks for the hashing.
    pub(super) fn verify(&self, username: &str, password: &str) -> bool {
        let password_ok = PasswordHash::new(&self.password_hash).is_ok_and(|hash| {
            Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        });
        let username_ok = secret::digest(username) == secret::digest(&self.username);
        password_ok && username_ok
    }

    /// The owner account in `dir`, if one exists.
    pub(super) fn load(dir: &Path) -> Result<Option<Self>, BrowserAuthError> {
        let path = dir.join(OWNER_FILE);
        let text = match secret::read_private(&path, MAX_OWNER_FILE_BYTES) {
            Ok(text) => text,
            Err(ReadError::Io(e)) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(ReadError::Io(source)) => return Err(BrowserAuthError::Io { path, source }),
            Err(ReadError::Exposed) => return Err(BrowserAuthError::Exposed(path)),
            Err(ReadError::TooLarge) => {
                return Err(BrowserAuthError::InvalidOwner(
                    path,
                    "the file is too large",
                ));
            }
        };
        let owner: Self = serde_json::from_str(&text)
            .map_err(|_| BrowserAuthError::InvalidOwner(path.clone(), "not an owner account"))?;
        if owner.version != VERSION {
            return Err(BrowserAuthError::InvalidOwner(
                path,
                "unsupported account format version",
            ));
        }
        if PasswordHash::new(&owner.password_hash).is_err() {
            return Err(BrowserAuthError::InvalidOwner(
                path,
                "the password hash is not a PHC string",
            ));
        }
        Ok(Some(owner))
    }

    /// Saves this account in `dir`, durably. Returns `false`, changing
    /// nothing, if an owner account already exists.
    pub(super) fn publish(&self, dir: &Path) -> io::Result<bool> {
        let temp = dir.join(OWNER_TEMP);
        // Left over only if an earlier attempt failed midway; the auth
        // directory's lock means no other writer uses it now.
        secret::remove(&temp)?;
        let mut file = secret::create_private(&temp)?;
        let text = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let linked = file
            .write_all(&text)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::hard_link(&temp, dir.join(OWNER_FILE)));
        secret::remove(&temp)?;
        match linked {
            Ok(()) => secret::sync_dir(dir).map(|()| true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Refuses an owner name or password that may not be used, saying why.
pub(super) fn validate(username: &str, password: &str) -> Result<(), ApiError> {
    let invalid = |message| Err(ApiError::new(ErrorCode::InvalidRequest, message));
    let chars = username.chars().count();
    if chars == 0 || chars > MAX_USERNAME_CHARS {
        return invalid("the owner name must be 1 to 64 characters");
    }
    if username.trim() != username || username.chars().any(char::is_control) {
        return invalid("the owner name may not have control characters or surrounding whitespace");
    }
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return invalid("the password must be at least 12 characters");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return invalid("the password must be at most 1024 bytes");
    }
    Ok(())
}

/// A setup secret that is in force.
#[derive(Debug)]
pub(super) struct SetupSecret {
    digest: [u8; 32],
    expires: Instant,
}

impl SetupSecret {
    /// Writes a fresh secret to `dir`, replacing any earlier one, valid
    /// for [`SETUP_LIFETIME`] from `now`.
    pub(super) fn issue(dir: &Path, now: Instant) -> io::Result<Self> {
        let path = dir.join(SETUP_FILE);
        let value = secret::generate()?;
        secret::remove(&path)?;
        let mut file = secret::create_private(&path)?;
        file.write_all(format!("{value}\n").as_bytes())?;
        file.sync_all()?;
        secret::sync_dir(dir)?;
        Ok(Self {
            digest: secret::digest(&value),
            expires: now + SETUP_LIFETIME,
        })
    }

    /// Whether the secret has expired at `now`.
    pub(super) fn expired(&self, now: Instant) -> bool {
        now >= self.expires
    }

    /// Makes the secret expire now.
    #[cfg(test)]
    pub(super) fn expire(&mut self) {
        self.expires = Instant::now();
    }

    /// Whether `given` is this secret. Surrounding whitespace is ignored,
    /// since the secret is often copied from a terminal.
    pub(super) fn matches(&self, given: &str) -> bool {
        secret::digest(given.trim()) == self.digest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_passwords_are_validated() {
        let good = "correct horse battery";
        assert!(validate("owner", good).is_ok());
        assert!(validate("Ünïcode owner", good).is_ok());
        assert!(validate("", good).is_err());
        assert!(validate(" owner", good).is_err());
        assert!(validate("own\ner", good).is_err());
        assert!(validate(&"o".repeat(65), good).is_err());
        assert!(validate("owner", "short").is_err());
        assert!(validate("owner", &"p".repeat(1025)).is_err());
        assert!(validate("owner", &"p".repeat(1024)).is_ok());
    }

    #[test]
    fn an_account_verifies_only_its_own_name_and_password() {
        let tmp = tempfile::tempdir().unwrap();
        let owner = Owner::new("owner".into(), "correct horse battery").unwrap();
        assert!(owner.password_hash.starts_with("$argon2id$"));
        assert!(!owner.password_hash.contains("correct"));
        assert!(owner.publish(tmp.path()).unwrap());

        let loaded = Owner::load(tmp.path()).unwrap().unwrap();
        assert!(loaded.verify("owner", "correct horse battery"));
        assert!(!loaded.verify("owner", "correct horse batter"));
        assert!(!loaded.verify("Owner", "correct horse battery"));

        // A second account never replaces the first.
        let other = Owner::new("other".into(), "another long password").unwrap();
        assert!(!other.publish(tmp.path()).unwrap());
        assert_eq!(Owner::load(tmp.path()).unwrap().unwrap().username, "owner");
        assert!(!tmp.path().join(OWNER_TEMP).exists());
    }

    #[cfg(unix)]
    #[test]
    fn account_files_are_owner_only_and_checked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        assert!(Owner::load(tmp.path()).unwrap().is_none());
        let path = tmp.path().join(OWNER_FILE);
        Owner::new("owner".into(), "correct horse battery")
            .unwrap()
            .publish(tmp.path())
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            Owner::load(tmp.path()),
            Err(BrowserAuthError::Exposed(_))
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"username":"o","password_hash":"plain"}"#,
        )
        .unwrap();
        assert!(matches!(
            Owner::load(tmp.path()),
            Err(BrowserAuthError::InvalidOwner(..))
        ));
    }

    #[test]
    fn a_setup_secret_is_written_owner_only_and_expires() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let first = SetupSecret::issue(tmp.path(), now).unwrap();
        let read = || std::fs::read_to_string(tmp.path().join(SETUP_FILE)).unwrap();
        let value = read();
        assert!(first.matches(&value));
        assert!(!first.matches(&value[1..]));
        assert!(!first.expired(now + SETUP_LIFETIME - Duration::from_secs(1)));
        assert!(first.expired(now + SETUP_LIFETIME));

        let second = SetupSecret::issue(tmp.path(), now).unwrap();
        assert!(!second.matches(&value), "a new secret replaces the old");
        assert!(second.matches(&read()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.path().join(SETUP_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
