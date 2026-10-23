//! Recognizing MEGA links without the network.
//!
//! A link names a file or a folder by its handle and carries the key in the
//! URL fragment, which is never sent in a request.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use url::Url;
use zeroize::Zeroizing;

use super::crypto::FileKey;

/// MEGA's hosts for shared links.
const HOSTS: &[&str] = &["mega.nz", "www.mega.nz", "mega.co.nz", "www.mega.co.nz"];

/// A node handle: 8 characters of URL-safe base64.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(String);

impl Handle {
    /// `raw` if it has a handle's shape.
    pub fn new(raw: &str) -> Option<Self> {
        (raw.len() == 8
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| Self(raw.to_owned()))
    }

    /// The handle.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Handle").field(&self.0).finish()
    }
}

/// A folder's key: 16 bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderKey(pub(super) Zeroizing<[u8; 16]>);

impl std::fmt::Debug for FolderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderKey([redacted])")
    }
}

/// What a MEGA link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MegaLink {
    /// One shared file.
    File {
        /// The file.
        handle: Handle,
        /// Its key.
        key: FileKey,
    },
    /// A shared folder, or a node inside it.
    Folder {
        /// The shared folder.
        handle: Handle,
        /// Its key.
        key: FolderKey,
        /// The node inside it the link points at, if any.
        node: Option<Inner>,
    },
}

/// A node inside a shared folder that a link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inner {
    /// A file.
    File(Handle),
    /// A subfolder.
    Folder(Handle),
}

impl MegaLink {
    /// What `link` names, if it is a MEGA file or folder link with its key:
    /// `/file/{handle}#{key}`, `/folder/{handle}#{key}` (optionally followed
    /// by `/file/{node}` or `/folder/{node}`), `/embed/{handle}#{key}`, or
    /// the older `#!{handle}!{key}` and `#F!{handle}!{key}`, on MEGA's own
    /// hosts only, without credentials or a custom port. A link without its
    /// key cannot be downloaded, so it is not recognized.
    pub fn parse(link: &Url) -> Option<Self> {
        if !matches!(link.scheme(), "http" | "https")
            || link.port().is_some()
            || !link.username().is_empty()
            || link.password().is_some()
        {
            return None;
        }
        let host = link.host_str()?;
        if !HOSTS.contains(&host.strip_suffix('.').unwrap_or(host)) {
            return None;
        }
        let fragment = link.fragment()?;
        let mut segments = link.path_segments()?.filter(|s| !s.is_empty());
        match (segments.next(), segments.next(), segments.next()) {
            (Some("file" | "embed"), Some(handle), None) => {
                Self::file(Handle::new(handle)?, fragment)
            }
            (Some("folder"), Some(handle), None) => {
                let (key, inner) = match fragment.split_once('/') {
                    Some((key, rest)) => (key, Some(rest)),
                    None => (fragment, None),
                };
                let node = match inner.map(|rest| rest.split_once('/')) {
                    None => None,
                    Some(Some(("file", node))) => Some(Inner::File(Handle::new(node)?)),
                    Some(Some(("folder", node))) => Some(Inner::Folder(Handle::new(node)?)),
                    Some(_) => return None,
                };
                Some(Self::Folder {
                    handle: Handle::new(handle)?,
                    key: folder_key(key)?,
                    node,
                })
            }
            (None | Some("embed"), None, None) => {
                let (kind, rest) = match fragment.strip_prefix("F!") {
                    Some(rest) => ('F', rest),
                    None => ('!', fragment.strip_prefix('!')?),
                };
                let (handle, key) = rest.split_once('!')?;
                let handle = Handle::new(handle)?;
                if kind == 'F' {
                    Some(Self::Folder {
                        handle,
                        key: folder_key(key)?,
                        node: None,
                    })
                } else {
                    Self::file(handle, key)
                }
            }
            _ => None,
        }
    }

    fn file(handle: Handle, key: &str) -> Option<Self> {
        let raw = decode::<32>(key)?;
        Some(Self::File {
            handle,
            key: FileKey::from_raw(&raw),
        })
    }

    /// The link to a file inside the shared folder `handle`, which carries
    /// everything needed to download it on its own.
    pub fn in_folder(handle: &Handle, key: &FolderKey, file: &Handle) -> Url {
        let mut url = Url::parse("https://mega.nz/folder/").expect("static URL");
        url.path_segments_mut()
            .expect("an HTTPS URL has a path")
            .pop()
            .push(handle.as_str());
        url.set_fragment(Some(&format!(
            "{}/file/{}",
            URL_SAFE_NO_PAD.encode(*key.0),
            file.as_str()
        )));
        url
    }
}

fn folder_key(raw: &str) -> Option<FolderKey> {
    decode::<16>(raw).map(FolderKey)
}

/// Exactly `N` bytes of unpadded URL-safe base64, also accepting the
/// trailing padding and the standard alphabet some links carry.
pub(super) fn decode<const N: usize>(raw: &str) -> Option<Zeroizing<[u8; N]>> {
    let normalized: Zeroizing<String> = Zeroizing::new(
        raw.trim_end_matches('=')
            .chars()
            .map(|c| match c {
                '+' => '-',
                '/' => '_',
                c => c,
            })
            .collect(),
    );
    let bytes = Zeroizing::new(URL_SAFE_NO_PAD.decode(normalized.as_bytes()).ok()?);
    let mut out = Zeroizing::new([0u8; N]);
    (bytes.len() == N).then(|| {
        out.copy_from_slice(&bytes);
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_KEY: &str = "ERMRFxETER8-P_sk6eh2pQECAwQFBgcIJibhP_X1aLo";
    const FOLDER_KEY: &str = "ZGVmZ2hpamtsbW5vcHFycw";

    fn parse(link: &str) -> Option<MegaLink> {
        MegaLink::parse(&Url::parse(link).unwrap())
    }

    fn file_handle(link: &str) -> Option<String> {
        match parse(link)? {
            MegaLink::File { handle, .. } => Some(handle.as_str().to_owned()),
            MegaLink::Folder { .. } => None,
        }
    }

    #[test]
    fn file_links_in_every_form_name_the_file() {
        for link in [
            format!("https://mega.nz/file/AbCd-_12#{FILE_KEY}"),
            format!("https://www.mega.nz/file/AbCd-_12#{FILE_KEY}"),
            format!("https://mega.co.nz/#!AbCd-_12!{FILE_KEY}"),
            format!("https://mega.nz/#!AbCd-_12!{FILE_KEY}"),
            format!("https://mega.nz/embed/AbCd-_12#{FILE_KEY}"),
            format!("https://mega.nz/embed#!AbCd-_12!{FILE_KEY}"),
            format!("https://mega.nz./file/AbCd-_12#{FILE_KEY}="),
        ] {
            assert_eq!(file_handle(&link).as_deref(), Some("AbCd-_12"), "{link}");
        }
    }

    #[test]
    fn folder_links_keep_the_node_they_point_at() {
        let folder = |link: &str| match parse(link) {
            Some(MegaLink::Folder { handle, node, .. }) => Some((handle.0, node)),
            _ => None,
        };
        let handle = || "FoLd3r_x".to_owned();
        assert_eq!(
            folder(&format!("https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}")),
            Some((handle(), None))
        );
        assert_eq!(
            folder(&format!("https://mega.nz/#F!FoLd3r_x!{FOLDER_KEY}")),
            Some((handle(), None))
        );
        assert_eq!(
            folder(&format!(
                "https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}/file/Ch1ld_aa"
            )),
            Some((
                handle(),
                Some(Inner::File(Handle::new("Ch1ld_aa").unwrap()))
            ))
        );
        assert_eq!(
            folder(&format!(
                "https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}/folder/Sub_f0ld"
            )),
            Some((
                handle(),
                Some(Inner::Folder(Handle::new("Sub_f0ld").unwrap()))
            ))
        );
    }

    #[test]
    fn links_without_a_usable_key_or_off_mega_are_not_recognized() {
        for link in [
            "https://mega.nz/file/AbCd-_12".to_owned(),
            "https://mega.nz/file/AbCd-_12#short".to_owned(),
            format!("https://mega.nz/file/AbCd-_12#{FOLDER_KEY}"),
            format!("https://mega.nz/folder/FoLd3r_x#{FILE_KEY}"),
            format!("https://mega.nz/file/toolonghandle#{FILE_KEY}"),
            format!("https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}/what/Ch1ld_aa"),
            format!("https://mega.nz:8443/file/AbCd-_12#{FILE_KEY}"),
            format!("https://user@mega.nz/file/AbCd-_12#{FILE_KEY}"),
            format!("https://notmega.nz/file/AbCd-_12#{FILE_KEY}"),
            format!("https://mega.io/file/AbCd-_12#{FILE_KEY}"),
            format!("ftp://mega.nz/file/AbCd-_12#{FILE_KEY}"),
            format!("https://mega.nz/file/AbCd-_12/x#{FILE_KEY}"),
        ] {
            assert_eq!(parse(&link), None, "{link}");
        }
    }

    #[test]
    fn a_file_in_a_folder_gets_a_link_of_its_own() {
        let Some(MegaLink::Folder { handle, key, .. }) =
            parse(&format!("https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}"))
        else {
            panic!("not a folder link");
        };
        let child = Handle::new("Ch1ld_aa").unwrap();
        let link = MegaLink::in_folder(&handle, &key, &child);
        assert_eq!(
            link.as_str(),
            format!("https://mega.nz/folder/FoLd3r_x#{FOLDER_KEY}/file/Ch1ld_aa")
        );
        assert_eq!(
            MegaLink::parse(&link),
            Some(MegaLink::Folder {
                handle,
                key,
                node: Some(Inner::File(child)),
            })
        );
    }
}
