//! File hosts: services whose shared links the engine downloads itself,
//! without an account or a plugin (ADR-08).
//!
//! A [`FileHost`] recognizes its links without the network, checks them for
//! the Links list, lists a shared folder's files, and turns a link into a
//! fresh download URL for each attempt. The engine owns the transfer: the
//! URL is fetched like any direct link, through the host's network policy,
//! and resumed under the identity the file host gives, since its URLs may
//! change from one attempt to the next. A host that encrypts files before
//! storing them supplies an [`Encoding`], which the engine applies while
//! writing and checks before publishing.

use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::query::CheckReason;

use crate::transfer::writer::BoxFuture;

/// Most files a folder check may list.
pub const MAX_FOLDER_FILES: usize = 1000;

/// One file host.
pub trait FileHost: Send + Sync + 'static {
    /// The service, as named in the UI's catalogue.
    fn service(&self) -> &ProviderId;

    /// Whether `link` is one of the host's shared links. Pure: no network.
    fn recognizes(&self, link: &Url) -> bool;

    /// What `link` is, for the Links list, without spending any transfer
    /// quota.
    fn check<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, HostCheck>;

    /// The file `link` names, with a download URL for this attempt.
    fn resolve<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, Result<HostedFile, HostError>>;

    /// What it means when a download URL it gave is refused with HTTP
    /// `status`, if the host gives that status a meaning of its own.
    fn refused(&self, _status: u16) -> Option<HostError> {
        None
    }
}

/// What a file host's check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostCheck {
    /// One file.
    File {
        /// Its name: an untrusted hint.
        name: Option<String>,
        /// Its size, when known.
        size: Option<u64>,
    },
    /// A shared folder: links to its files, each one the host recognizes,
    /// at most [`MAX_FOLDER_FILES`].
    Folder(Vec<Url>),
    /// The host says it is gone.
    Offline(String),
    /// The check could not tell.
    Unknown(CheckReason, String),
}

/// A file a host will serve to this attempt.
#[derive(Clone)]
pub struct HostedFile {
    /// Where its bytes are, for this attempt only: never stored or shown.
    pub url: Url,
    /// Its name: an untrusted hint, checked like a server's suggestion.
    pub name: Option<String>,
    /// Its size as stored, which the download must match.
    pub size: u64,
    /// A stable identity of the file and its content, so saved bytes are
    /// continued only for the same file. It names no secret.
    pub identity: String,
    /// How its stored bytes become the file, if they are not the file.
    pub encoding: Option<Arc<dyn Encoding>>,
}

impl std::fmt::Debug for HostedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostedFile")
            .field("url", &"[redacted]")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("identity", &self.identity)
            .field("encoding", &self.encoding.is_some())
            .finish()
    }
}

/// Turns a hosted file's stored bytes into the file, from any offset, and
/// proves the result whole. Each stored byte becomes one file byte.
pub trait Encoding: Send + Sync + 'static {
    /// A decoder for the bytes from `offset` on.
    fn decoder(&self, offset: u64) -> Box<dyn Decoder>;

    /// A check of the whole decoded file, fed from its first byte.
    fn verifier(&self) -> Box<dyn Verifier>;
}

/// Decodes consecutive bytes in place.
pub trait Decoder: Send {
    /// Decodes the next bytes.
    fn decode(&mut self, data: &mut [u8]);
}

/// Checks a decoded file fed to it in order.
pub trait Verifier: Send {
    /// Feeds the next bytes.
    fn update(&mut self, data: &[u8]);

    /// Whether everything fed is the file the host stored.
    fn verify(self: Box<Self>) -> bool;
}

/// Why a host gave no download URL. The message says what the person can
/// do and names no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    /// The failure to record.
    pub code: FailureCode,
    /// What happened.
    pub message: String,
    /// When trying again may work, if the host said.
    pub retry_after: Option<Duration>,
}

impl HostError {
    /// A failure with no hint of when to try again.
    pub fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after: None,
        }
    }
}

/// The file hosts a host registered.
#[derive(Default, Clone)]
pub(crate) struct Hosts(Arc<[Arc<dyn FileHost>]>);

impl Hosts {
    pub(crate) fn new(hosts: Vec<Arc<dyn FileHost>>) -> Self {
        Self(hosts.into())
    }

    /// The file host that recognizes `link`, if any.
    pub(crate) fn recognize(&self, link: &Url) -> Option<&Arc<dyn FileHost>> {
        self.0.iter().find(|h| h.recognizes(link))
    }
}
