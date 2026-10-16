//! Encryption of stored provider credentials (architecture brief section
//! 8.4, ADR-24).
//!
//! Credentials are sealed with ChaCha20-Poly1305 (`ring`) under a key kept
//! in a file outside the state directory, so the database, its WAL, and a
//! copy of the state directory hold only ciphertext. Each record has a
//! fresh random 96-bit nonce, and its associated data binds the record
//! format, the account's opaque secret reference, and the service, so a
//! record moved to another account or service fails to open.
//!
//! The key file holds 32 random bytes as 64 hexadecimal digits. The two
//! keys in use are derived from it with HMAC-SHA-256 under fixed labels:
//! one encrypts, and one yields the key check the database stores, which
//! tells a wrong key from a right one without decrypting anything and
//! reveals nothing about the key.
//!
//! Provisioning and restart:
//!
//! * No key file configured: accounts cannot be added
//!   ([`VaultState::NotConfigured`]).
//! * Configured, and the database never stored a key check: the vault is
//!   uninitialized. Adding the first account reads the file, or creates it
//!   owner-only if it does not exist, and stores its check.
//! * The database stores a check: the file must exist, be owner-only, and
//!   hold the key with that check. Anything else locks the vault
//!   ([`VaultState::Locked`]) with the reason; the engine never generates
//!   a replacement, which would make the stored credentials unreadable.
//!   Downloads without an account are not affected.
//!
//! Rotating the key, and recovering from losing it, are not supported:
//! with the key gone, the accounts must be removed and signed in again.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_protocol::account::VaultState;
use wayfetch_protocol::{ErrorCode, ProtocolError};
use zeroize::Zeroizing;

use super::Secret;

/// Bytes of key material in the key file.
const KEY_BYTES: usize = 32;
/// Largest key file read: the hex digits and some whitespace.
const MAX_KEY_FILE_BYTES: u64 = 256;
/// Bytes of an opaque secret reference.
pub(crate) const REFERENCE_BYTES: usize = 16;
/// Largest credential sealed, in bytes.
pub(crate) const MAX_CREDENTIAL_BYTES: usize = 8 * 1024;
/// The sealed-record format this engine writes and reads.
pub(crate) const FORMAT: u8 = 1;
/// Bytes a sealed record adds to its plaintext: the nonce and the tag.
pub(crate) const SEAL_OVERHEAD: usize = NONCE_LEN + 16;

/// The key check stored in the database.
pub(crate) type KeyCheck = [u8; 32];

/// An opaque reference naming one stored credential record.
pub(crate) type Reference = [u8; REFERENCE_BYTES];

/// The keys derived from the key file.
pub(crate) struct Keys {
    aead: LessSafeKey,
    check: KeyCheck,
    random: SystemRandom,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys([redacted])")
    }
}

/// A credential record could not be sealed or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SealError {
    /// The credential is larger than [`MAX_CREDENTIAL_BYTES`].
    #[error("the credential is too large to store")]
    TooLarge,
    /// The record was changed, belongs to another account, or was sealed
    /// with another key.
    #[error("the stored credentials failed their integrity check")]
    Tampered,
    /// The system's random generator failed.
    #[error("no random numbers are available")]
    Random,
}

impl Keys {
    /// Derives the keys from the key file's material.
    fn derive(material: &[u8; KEY_BYTES]) -> Self {
        let master = hmac::Key::new(hmac::HMAC_SHA256, material);
        let label = |name: &[u8]| Zeroizing::new(hmac::sign(&master, name).as_ref().to_vec());
        let encryption = label(b"wayfetch credential encryption v1");
        let check = hmac::sign(&master, b"wayfetch credential key check v1");
        Self {
            aead: LessSafeKey::new(
                UnboundKey::new(&CHACHA20_POLY1305, &encryption).expect("32-byte key"),
            ),
            check: check.as_ref().try_into().expect("SHA-256 output"),
            random: SystemRandom::new(),
        }
    }

    /// The check the database stores for this key.
    pub(crate) fn check(&self) -> KeyCheck {
        self.check
    }

    /// A fresh random reference for a new credential record.
    pub(crate) fn reference(&self) -> Result<Reference, SealError> {
        let mut reference = [0; REFERENCE_BYTES];
        self.random
            .fill(&mut reference)
            .map_err(|_| SealError::Random)?;
        Ok(reference)
    }

    /// Seals `credential` for the record `reference` of `service`: the
    /// nonce, then the ciphertext and its tag.
    pub(crate) fn seal(
        &self,
        reference: &Reference,
        service: &ProviderId,
        credential: &Secret,
    ) -> Result<Vec<u8>, SealError> {
        self.seal_as(Purpose::Credential, reference, service, credential)
    }

    /// Seals what a provider operation returned, such as a generated
    /// download link, for the record `reference` of `service`. Such a
    /// record never opens as a credential, nor a credential as one.
    pub(crate) fn seal_result(
        &self,
        reference: &Reference,
        service: &ProviderId,
        result: &Secret,
    ) -> Result<Vec<u8>, SealError> {
        self.seal_as(Purpose::ProviderResult, reference, service, result)
    }

    /// Opens a record [`Self::seal_result`] produced.
    pub(crate) fn open_result(
        &self,
        reference: &Reference,
        service: &ProviderId,
        sealed: &[u8],
    ) -> Result<Secret, SealError> {
        self.open_as(Purpose::ProviderResult, reference, service, sealed)
    }

    fn seal_as(
        &self,
        purpose: Purpose,
        reference: &Reference,
        service: &ProviderId,
        credential: &Secret,
    ) -> Result<Vec<u8>, SealError> {
        if credential.len() > MAX_CREDENTIAL_BYTES {
            return Err(SealError::TooLarge);
        }
        let mut nonce = [0; NONCE_LEN];
        self.random
            .fill(&mut nonce)
            .map_err(|_| SealError::Random)?;
        let mut sealed = Vec::with_capacity(SEAL_OVERHEAD + credential.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(credential.expose());
        let (head, body) = sealed.split_at_mut(NONCE_LEN);
        let tag = self
            .aead
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(head.try_into().expect("nonce length")),
                Aad::from(associated(purpose, reference, service)),
                body,
            )
            .map_err(|_| SealError::TooLarge)?;
        sealed.extend_from_slice(tag.as_ref());
        Ok(sealed)
    }

    /// Opens a record [`Self::seal`] produced for the same reference and
    /// service.
    pub(crate) fn open(
        &self,
        reference: &Reference,
        service: &ProviderId,
        sealed: &[u8],
    ) -> Result<Secret, SealError> {
        self.open_as(Purpose::Credential, reference, service, sealed)
    }

    fn open_as(
        &self,
        purpose: Purpose,
        reference: &Reference,
        service: &ProviderId,
        sealed: &[u8],
    ) -> Result<Secret, SealError> {
        if sealed.len() < SEAL_OVERHEAD || sealed.len() > SEAL_OVERHEAD + MAX_CREDENTIAL_BYTES {
            return Err(SealError::Tampered);
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let mut body = Zeroizing::new(body.to_vec());
        let plain = self
            .aead
            .open_in_place(
                Nonce::assume_unique_for_key(nonce.try_into().expect("nonce length")),
                Aad::from(associated(purpose, reference, service)),
                &mut body,
            )
            .map_err(|_| SealError::Tampered)?;
        Ok(Secret::new(plain.to_vec()))
    }
}

/// What a sealed record holds.
#[derive(Debug, Clone, Copy)]
enum Purpose {
    /// An account's credentials.
    Credential,
    /// What a provider operation returned.
    ProviderResult,
}

/// The associated data binding a record to what it holds, its format,
/// reference, and service.
fn associated(purpose: Purpose, reference: &Reference, service: &ProviderId) -> Vec<u8> {
    let mut aad = match purpose {
        Purpose::Credential => b"wayfetch credential\0".to_vec(),
        Purpose::ProviderResult => b"wayfetch provider result\0".to_vec(),
    };
    aad.push(FORMAT);
    aad.extend_from_slice(reference);
    aad.extend_from_slice(service.as_str().as_bytes());
    aad
}

/// Whether stored credentials can be used, and with which keys.
#[derive(Debug, Clone)]
pub(crate) enum Vault {
    /// No key file is configured.
    NotConfigured,
    /// A key file is configured, and no key was ever stored.
    Uninitialized(PathBuf),
    /// The key is loaded and matches the stored check.
    Ready(Arc<Keys>),
    /// A key was stored, and the configured file cannot provide it.
    Locked(String),
}

impl Vault {
    /// The vault for the configured key file and the check the database
    /// stores. Blocking: reads the file. Never creates it.
    pub(crate) fn open(path: Option<&Path>, stored: Option<KeyCheck>) -> Self {
        let (Some(path), Some(stored)) = (path, stored) else {
            return match (path, stored) {
                (Some(path), None) => Self::Uninitialized(path.to_owned()),
                (None, Some(_)) => Self::Locked(
                    "accounts were stored with a credential key, and none is configured".into(),
                ),
                _ => Self::NotConfigured,
            };
        };
        match read_key(path) {
            Ok(Some(keys)) if keys.check == stored => Self::Ready(Arc::new(keys)),
            Ok(Some(_)) => Self::Locked(format!(
                "the credential key in {} is not the key the accounts were stored with",
                path.display()
            )),
            Ok(None) => Self::Locked(format!(
                "the credential key file {} is missing",
                path.display()
            )),
            Err(problem) => Self::Locked(problem),
        }
    }

    /// Loads the configured key for a vault that never stored one, first
    /// creating the key file, owner-only, if it does not exist. Blocking.
    /// The caller stores the returned keys' check before using them.
    pub(crate) fn initialize(path: &Path) -> Result<Keys, String> {
        if let Some(keys) = read_key(path)? {
            return Ok(keys);
        }
        create_key(path).map_err(|e| {
            format!(
                "the credential key file {} could not be created: {e}",
                path.display()
            )
        })
    }

    /// The state clients see, and why it is not ready.
    pub(crate) fn state(&self) -> (VaultState, Option<DiagnosticText>) {
        match self {
            Self::Ready(_) | Self::Uninitialized(_) => (VaultState::Ready, None),
            Self::NotConfigured => (
                VaultState::NotConfigured,
                Some(DiagnosticText::new(
                    "no credential key is configured for this installation, so accounts cannot be stored",
                )),
            ),
            Self::Locked(problem) => (VaultState::Locked, Some(DiagnosticText::new(problem))),
        }
    }

    /// The keys, or why accounts cannot be used.
    pub(crate) fn keys(&self) -> Result<Arc<Keys>, ProtocolError> {
        match self {
            Self::Ready(keys) => Ok(keys.clone()),
            Self::Uninitialized(_) => Err(ProtocolError::new(
                ErrorCode::NotFound,
                "no account credentials are stored",
            )),
            Self::NotConfigured | Self::Locked(_) => {
                let (_, problem) = self.state();
                Err(ProtocolError::new(
                    ErrorCode::NotApplicable,
                    &format!(
                        "credential storage is unavailable: {}",
                        problem.map(String::from).unwrap_or_default()
                    ),
                ))
            }
        }
    }
}

/// Reads the key file at `path`: `None` if it does not exist.
fn read_key(path: &Path) -> Result<Option<Keys>, String> {
    let shown = path.display();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "the credential key file {shown} cannot be read: {e}"
            ));
        }
    };
    if !is_private(&file).map_err(|e| format!("the credential key file {shown}: {e}"))? {
        return Err(format!(
            "the credential key file {shown} is accessible to other users; make it owner-only (mode 600)"
        ));
    }
    let mut text = Zeroizing::new(String::new());
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("the credential key file {shown} cannot be read: {e}"))?;
    let material = (text.len() as u64 <= MAX_KEY_FILE_BYTES)
        .then(|| decode_hex(text.trim()))
        .flatten()
        .ok_or_else(|| {
            format!("the credential key file {shown} does not hold a key of 64 hexadecimal digits")
        })?;
    Ok(Some(Keys::derive(&material)))
}

/// Creates a new key file at `path` and returns its keys. Refuses to
/// replace a file that appeared meanwhile.
fn create_key(path: &Path) -> io::Result<Keys> {
    let mut material = Zeroizing::new([0; KEY_BYTES]);
    SystemRandom::new()
        .fill(material.as_mut())
        .map_err(|_| io::Error::other("no random numbers are available"))?;
    let mut text = Zeroizing::new(String::with_capacity(KEY_BYTES * 2 + 1));
    for byte in material.iter() {
        text.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        text.push(char::from(b"0123456789abcdef"[usize::from(byte & 15)]));
    }
    text.push('\n');
    let mut file = create_private(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    // The new name is durable too.
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(Keys::derive(&material))
}

fn decode_hex(text: &str) -> Option<Zeroizing<[u8; KEY_BYTES]>> {
    let digits = text.as_bytes();
    if digits.len() != KEY_BYTES * 2 {
        return None;
    }
    let value = |d: u8| char::from(d).to_digit(16);
    let mut out = Zeroizing::new([0; KEY_BYTES]);
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        *byte = u8::try_from(value(pair[0])? << 4 | value(pair[1])?).ok()?;
    }
    Some(out)
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// Whether only the file's owner may read or write it.
#[cfg(unix)]
fn is_private(file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.mode() & 0o077 == 0)
}

/// Other platforms' permissions are not checked here; the file still
/// lives outside the state directory.
#[cfg(not(unix))]
fn is_private(_file: &File) -> io::Result<bool> {
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> ProviderId {
        ProviderId::new("real-debrid").unwrap()
    }

    #[test]
    fn a_new_key_file_is_owner_only_and_reopens_with_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.key");
        assert!(matches!(
            Vault::open(Some(&path), None),
            Vault::Uninitialized(_)
        ));
        let keys = Vault::initialize(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The same file gives the same keys; nothing is replaced.
        assert_eq!(Vault::initialize(&path).unwrap().check(), keys.check());
        assert!(matches!(
            Vault::open(Some(&path), Some(keys.check())),
            Vault::Ready(_)
        ));
    }

    #[test]
    fn a_missing_wrong_or_exposed_key_locks_without_replacing_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential.key");
        let check = Vault::initialize(&path).unwrap().check();
        let locked = |vault: Vault| match vault {
            Vault::Locked(problem) => problem,
            other => panic!("{other:?}"),
        };

        std::fs::remove_file(&path).unwrap();
        assert!(locked(Vault::open(Some(&path), Some(check))).contains("missing"));
        assert!(!path.exists(), "a missing key is never recreated");

        let other = Vault::initialize(&path).unwrap();
        assert_ne!(other.check(), check);
        assert!(locked(Vault::open(Some(&path), Some(check))).contains("not the key"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(locked(Vault::open(Some(&path), Some(other.check()))).contains("owner-only"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        std::fs::write(&path, "not a key\n").unwrap();
        assert!(locked(Vault::open(Some(&path), Some(check))).contains("64 hexadecimal"));
        assert!(locked(Vault::open(None, Some(check))).contains("none is configured"));
        assert!(matches!(Vault::open(None, None), Vault::NotConfigured));
    }

    #[test]
    fn sealed_records_open_only_where_they_were_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let keys = Vault::initialize(&dir.path().join("k")).unwrap();
        let reference = keys.reference().unwrap();
        let secret = Secret::from_text("synthetic-refresh-token-0123456789");
        let sealed = keys.seal(&reference, &service(), &secret).unwrap();
        assert!(
            !sealed.windows(secret.len()).any(|w| w == secret.expose()),
            "ciphertext holds the plaintext"
        );
        assert_eq!(keys.open(&reference, &service(), &sealed).unwrap(), secret);
        // Fresh nonce each time.
        assert_ne!(keys.seal(&reference, &service(), &secret).unwrap(), sealed);

        let mut flipped = sealed.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert_eq!(
            keys.open(&reference, &service(), &flipped).unwrap_err(),
            SealError::Tampered
        );
        let elsewhere = keys.reference().unwrap();
        assert!(keys.open(&elsewhere, &service(), &sealed).is_err());
        let other_service = ProviderId::new("alldebrid").unwrap();
        assert!(keys.open(&reference, &other_service, &sealed).is_err());
        let other_key = Vault::initialize(&dir.path().join("k2")).unwrap();
        assert!(other_key.open(&reference, &service(), &sealed).is_err());
        // A credential never opens as a provider result, nor one as the other.
        assert!(keys.open_result(&reference, &service(), &sealed).is_err());
        let result = keys.seal_result(&reference, &service(), &secret).unwrap();
        assert_eq!(
            keys.open_result(&reference, &service(), &result).unwrap(),
            secret
        );
        assert!(keys.open(&reference, &service(), &result).is_err());
        assert!(keys.open(&reference, &service(), &sealed[..10]).is_err());

        let huge = Secret::new(vec![0; MAX_CREDENTIAL_BYTES + 1]);
        assert_eq!(
            keys.seal(&reference, &service(), &huge).unwrap_err(),
            SealError::TooLarge
        );
    }
}
