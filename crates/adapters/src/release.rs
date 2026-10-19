//! Official signed releases of a plugin, which Settings installs in place
//! of the copy found (ADR-26 amendment).
//!
//! A release publishes a checksum manifest and a detached OpenPGP
//! signature of it. The signature must verify against the signing key
//! pinned here, through the `pgp` crate; then the manifest's digest for
//! the one file chosen for this system must match the downloaded bytes.
//! Either failing refuses the release: there is no fallback to the
//! checksums alone. Files are fetched through the host's transport, so
//! the egress policy and TLS settings apply to every redirect.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};
use pgp::packet::SignatureType;
use pgp::types::KeyDetails;
use sha2::{Digest, Sha256};
use url::Url;
use wayfetch_engine::media::PluginError;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Reach, Transport};

/// Longest wait for a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest wait for the next part of a file.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Largest checksum manifest or signature read.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;

/// A release channel and the key that signs it.
#[derive(Debug)]
pub struct Release {
    /// Where the newest release's files are, ending in `/`.
    pub base: &'static str,
    /// The checksum manifest's file name: `sha256  name` lines.
    pub manifest: &'static str,
    /// Its detached signature's file name.
    pub signature: &'static str,
    /// The signing key, ASCII-armored.
    pub key: &'static str,
    /// The signing key's fingerprint, in upper-case hex.
    pub fingerprint: &'static str,
    /// The file to install on this system, given the copy in use.
    pub asset: fn(current: Option<&Path>) -> Option<Asset>,
    /// Largest file installed.
    pub max_bytes: u64,
}

/// A file of a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// Its name in the manifest.
    pub name: &'static str,
    /// Whether it is a Python zip application, run by the interpreter its
    /// first line names.
    pub zipapp: bool,
}

/// yt-dlp's stable releases on GitHub, signed by the yt-dlp signing key
/// published as `public.key` in its repository.
pub const YT_DLP: Release = Release {
    base: "https://github.com/yt-dlp/yt-dlp/releases/latest/download/",
    manifest: "SHA2-256SUMS",
    signature: "SHA2-256SUMS.sig",
    key: include_str!("keys/yt-dlp.asc"),
    fingerprint: "AC0CBBE6848D6A873464AF4E57CF65933B5A7581",
    asset: yt_dlp_asset,
    max_bytes: 128 * 1024 * 1024,
};

/// The yt-dlp build for this system: the zip application, run by the same
/// Python as the copy in use, when that copy is one; else the standalone
/// build for this platform, which needs no Python.
fn yt_dlp_asset(current: Option<&Path>) -> Option<Asset> {
    if current.is_some_and(is_zipapp) {
        return Some(Asset {
            name: "yt-dlp",
            zipapp: true,
        });
    }
    let musl = cfg!(target_env = "musl");
    let name = match (std::env::consts::OS, std::env::consts::ARCH, musl) {
        ("linux", "x86_64", false) => "yt-dlp_linux",
        ("linux", "aarch64", false) => "yt-dlp_linux_aarch64",
        ("linux", "x86_64", true) => "yt-dlp_musllinux",
        ("linux", "aarch64", true) => "yt-dlp_musllinux_aarch64",
        ("macos", _, _) => "yt-dlp_macos",
        ("windows", "x86_64", _) => "yt-dlp.exe",
        ("windows", "aarch64", _) => "yt-dlp_arm64.exe",
        ("windows", "x86", _) => "yt-dlp_x86.exe",
        _ => return None,
    };
    Some(Asset {
        name,
        zipapp: false,
    })
}

/// Whether `path` is a Python zip application: an interpreter line, then
/// a zip archive whose end record closes the file.
pub fn is_zipapp(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(b"#!")
        && bytes.len() >= 22
        && bytes[bytes.len() - 22..].starts_with(b"PK\x05\x06")
}

/// `zipapp` with its interpreter line replaced by `current`'s, so it runs
/// with the same Python. Zip readers locate entries from the archive's
/// end, so the archive is unchanged.
pub fn with_interpreter_of(current: &[u8], zipapp: &[u8]) -> Option<Vec<u8>> {
    let line = |b: &[u8]| {
        b.starts_with(b"#!")
            .then(|| b.iter().position(|&c| c == b'\n'))
            .flatten()
            .map(|end| end + 1)
    };
    let (keep, drop) = (line(current)?, line(zipapp)?);
    let mut out = current[..keep].to_vec();
    out.extend_from_slice(&zipapp[drop..]);
    Some(out)
}

impl Release {
    /// The URL of the newest release's file `name`.
    pub fn url(&self, name: &str) -> Result<Url, PluginError> {
        Url::parse(self.base)
            .and_then(|base| base.join(name))
            .map_err(|e| PluginError::NotApplicable(format!("bad release address: {e}")))
    }

    /// Fetches the newest release's manifest and signature, and returns
    /// the manifest's digest and its entries once the signature verifies.
    pub async fn manifest(
        &self,
        transport: &dyn Transport,
    ) -> Result<([u8; 32], BTreeMap<String, [u8; 32]>), PluginError> {
        let mut manifest = Vec::new();
        let digest = fetch(
            transport,
            &self.url(self.manifest)?,
            MAX_MANIFEST_BYTES,
            &mut manifest,
        )
        .await?;
        let mut signature = Vec::new();
        fetch(
            transport,
            &self.url(self.signature)?,
            MAX_MANIFEST_BYTES,
            &mut signature,
        )
        .await?;
        Ok((digest, self.verify(&manifest, &signature)?))
    }

    /// The entries of `manifest` if `signature` is a valid binary
    /// signature of it by the pinned key.
    pub fn verify(
        &self,
        manifest: &[u8],
        signature: &[u8],
    ) -> Result<BTreeMap<String, [u8; 32]>, PluginError> {
        let rejected = |why: &str| PluginError::Rejected(format!("the release's signature {why}"));
        let (key, _) = SignedPublicKey::from_string(self.key)
            .map_err(|_| rejected("cannot be checked: the pinned key is unreadable"))?;
        if format!("{:X}", key.fingerprint()) != self.fingerprint {
            return Err(rejected(
                "cannot be checked: the pinned key is not the expected one",
            ));
        }
        let signature =
            DetachedSignature::from_bytes(signature).map_err(|_| rejected("is unreadable"))?;
        if signature.signature.typ() != Some(SignatureType::Binary) {
            return Err(rejected("is not a file signature"));
        }
        signature
            .verify(&key, manifest)
            .map_err(|_| rejected("is not valid for its checksums"))?;
        parse_manifest(manifest).ok_or_else(|| rejected("covers an unreadable checksum list"))
    }
}

/// `sha256  name` lines; `None` if any line is not one.
fn parse_manifest(manifest: &[u8]) -> Option<BTreeMap<String, [u8; 32]>> {
    let text = std::str::from_utf8(manifest).ok()?;
    let mut entries = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (digest, name) = line.split_once(char::is_whitespace)?;
        let mut bytes = [0; 32];
        hex::decode_to_slice(digest, &mut bytes).ok()?;
        // `*` marks a binary-mode entry in sha256sum's format.
        let name = name.trim_start().trim_start_matches('*');
        entries.insert(name.to_owned(), bytes);
    }
    Some(entries)
}

/// Fetches all of `url` into `out`, at most `max` bytes, and returns the
/// SHA-256 digest of what was written.
pub async fn fetch(
    transport: &dyn Transport,
    url: &Url,
    max: u64,
    out: &mut impl Save,
) -> Result<[u8; 32], PluginError> {
    let unreachable = |why: String| {
        PluginError::Unreachable(format!("{} could not be fetched: {why}", file_name(url)))
    };
    let request = OpenRequest {
        url: url.clone(),
        range: None,
        validators: None,
        connect_timeout: CONNECT_TIMEOUT,
        read_buffer: 64 * 1024,
        user_agent: None,
        reach: Reach::Allowed,
    };
    let response = tokio::time::timeout(CONNECT_TIMEOUT * 2, transport.open(&request))
        .await
        .map_err(|_| unreachable("no answer in time".into()))?
        .map_err(|e| unreachable(e.message))?;
    match response.framing {
        Framing::Full { length } if length.is_none_or(|l| l <= max) => {}
        Framing::Full { .. } => return Err(unreachable(format!("it is larger than {max} bytes"))),
        _ => return Err(unreachable("the server answered part of it".into())),
    }
    let mut body = response.body;
    let (mut hasher, mut read) = (Sha256::new(), 0u64);
    loop {
        let chunk = tokio::time::timeout(READ_TIMEOUT, body.next_chunk())
            .await
            .map_err(|_| unreachable("the download stalled".into()))?
            .map_err(|e| unreachable(e.to_string()))?;
        let Some(chunk) = chunk else { break };
        read += chunk.len() as u64;
        if read > max {
            return Err(unreachable(format!("it is larger than {max} bytes")));
        }
        hasher.update(&chunk);
        out.save(chunk).await?;
    }
    Ok(hasher.finalize().into())
}

/// Where [`fetch`] puts what it reads.
pub trait Save: Send {
    /// Keeps `chunk`, the next bytes read.
    fn save(&mut self, chunk: Bytes) -> impl Future<Output = Result<(), PluginError>> + Send;
}

impl Save for Vec<u8> {
    async fn save(&mut self, chunk: Bytes) -> Result<(), PluginError> {
        self.extend_from_slice(&chunk);
        Ok(())
    }
}

/// Chunks for a writer on another thread, which stops taking them if
/// writing fails.
impl Save for tokio::sync::mpsc::Sender<Bytes> {
    async fn save(&mut self, chunk: Bytes) -> Result<(), PluginError> {
        self.send(chunk)
            .await
            .map_err(|_| PluginError::Storage("the download could not be saved".into()))
    }
}

/// The last path segment of `url`, for messages.
fn file_name(url: &Url) -> &str {
    url.path_segments()
        .and_then(Iterator::last)
        .unwrap_or("the release")
}

/// Whether version `new` is newer than `old`: compared as dot-separated
/// numbers when both are, such as yt-dlp's `2026.08.19`; otherwise only
/// a different version counts as newer.
pub fn newer(new: &str, old: &str) -> bool {
    let numbers = |v: &str| {
        v.split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    };
    match (numbers(new), numbers(old)) {
        (Some(new), Some(old)) => new > old,
        _ => new != old,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &[u8] = include_bytes!("../tests/fixtures/yt-dlp-2026.08.19/SHA2-256SUMS");
    const SIGNATURE: &[u8] = include_bytes!("../tests/fixtures/yt-dlp-2026.08.19/SHA2-256SUMS.sig");

    #[test]
    fn the_real_signed_manifest_verifies_and_lists_each_build() {
        let entries = YT_DLP.verify(MANIFEST, SIGNATURE).unwrap();
        assert_eq!(
            hex::encode(entries["yt-dlp"]),
            "1fa6733c37ea6fb51c99ad8fe785e7b7e5f3246c9b980230329d4fb72ed8d4d6"
        );
        assert!(entries.contains_key("yt-dlp_linux"));
        assert!(entries.contains_key("yt-dlp.exe"));
    }

    #[test]
    fn a_changed_manifest_or_signature_or_key_is_refused() {
        let rejected = |r: Result<_, PluginError>| matches!(r, Err(PluginError::Rejected(_)));
        let mut manifest = MANIFEST.to_vec();
        manifest[0] = if manifest[0] == b'0' { b'1' } else { b'0' };
        assert!(rejected(YT_DLP.verify(&manifest, SIGNATURE)));
        let mut signature = SIGNATURE.to_vec();
        let last = signature.len() - 1;
        signature[last] ^= 1;
        assert!(rejected(YT_DLP.verify(MANIFEST, &signature)));
        assert!(rejected(YT_DLP.verify(MANIFEST, b"not a signature")));
        let other = Release {
            fingerprint: "0000000000000000000000000000000000000000",
            ..YT_DLP
        };
        assert!(rejected(other.verify(MANIFEST, SIGNATURE)));
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(newer("2026.09.01", "2026.08.19"));
        assert!(newer("2026.08.19.120000", "2026.08.19"));
        assert!(!newer("2026.08.19", "2026.08.19"));
        assert!(!newer("2026.08.01", "2026.08.19"));
        assert!(newer("x", "y"));
        assert!(!newer("x", "x"));
    }

    #[test]
    fn a_zipapp_keeps_the_interpreter_of_the_copy_in_use() {
        let replaced = with_interpreter_of(
            b"#!/usr/bin/python3\nPK old",
            b"#!/usr/bin/env python3\nPK new",
        );
        assert_eq!(
            replaced.as_deref(),
            Some(&b"#!/usr/bin/python3\nPK new"[..])
        );
        assert_eq!(with_interpreter_of(b"PK", b"#!/x\nPK"), None);

        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("zip");
        std::fs::write(
            &zip,
            b"#!/usr/bin/python3\nPK\x03\x04...PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
        )
        .unwrap();
        assert!(is_zipapp(&zip));
        assert_eq!((YT_DLP.asset)(Some(&zip)).map(|a| a.name), Some("yt-dlp"));
        let binary = dir.path().join("binary");
        std::fs::write(&binary, b"\x7fELF").unwrap();
        assert!(!is_zipapp(&binary));
        assert!((YT_DLP.asset)(Some(&binary)).is_none_or(|a| !a.zipapp));
    }
}
