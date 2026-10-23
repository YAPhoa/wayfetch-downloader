//! Dropbox: public shared links to single files, through Dropbox's site.
//!
//! A file link with `dl=1` redirects to a temporary download URL that names
//! the file and accepts ranges. A check asks for its first byte, as direct
//! links are checked, and each attempt asks again for a fresh URL. A link
//! can be pointed at another file of the same name and size, so saved bytes
//! are continued only under the same `ETag` or `Last-Modified`. No
//! account or app key is used, so shared folders and password-protected
//! links are not supported: Dropbox serves those only as a page or a zip.

use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest, Sha256};
use url::Url;
use wayfetch_engine::accounts::provider::ApiClient;
use wayfetch_engine::crawl::{InspectMethod, InspectRequest, Inspection, Inspector, is_page};
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::name::ProviderId;
use wayfetch_engine::hosting::{FileHost, HostCheck, HostError, HostedFile};
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::transport::Reach;
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::common::{self, Endpoint};

/// The service's ID, as in the UI's catalogue.
pub const SERVICE: &str = "dropbox";
/// Dropbox's site, which serves shared links.
pub const OFFICIAL_SITE: &str = "https://www.dropbox.com/";
/// Hosts whose paths are shared links.
const HOSTS: &[&str] = &[
    "www.dropbox.com",
    "dropbox.com",
    "dl.dropbox.com",
    "dl.dropboxusercontent.com",
];
/// Longest path segment of a link accepted.
const MAX_SEGMENT: usize = 512;
/// Deadline for asking Dropbox about a link.
const DEADLINE: Duration = Duration::from_secs(20);
/// Largest page read: none is, since a page is never the file.
const MAX_PAGE: usize = 64 * 1024;
/// How long to wait after Dropbox fails.
const BACK_OFF: Duration = Duration::from_secs(60);
/// How long Dropbox suspends a link that drew too much traffic, at least.
const TRAFFIC_BAN: Duration = Duration::from_secs(24 * 60 * 60);

const NOT_FOUND: &str = "Dropbox says this link does not exist or was removed.";
const TRAFFIC: &str = "Dropbox has suspended this link for drawing too much traffic today. \
     Dropbox lifts it after a day or more; retry the download later.";
const PAGE: &str = "Dropbox answered with a web page instead of the file: the link may need a \
     password or a sign-in, which Wayfetch does not support for Dropbox yet.";
const FOLDER: &str = "This is a Dropbox folder link. Wayfetch downloads single Dropbox files for \
     now; add a link to each file instead.";

/// Which kind of thing a shared link names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One file.
    File,
    /// A shared folder.
    Folder,
}

/// A Dropbox shared link, without its download switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropboxLink {
    /// What it names.
    pub kind: Kind,
    /// Its path on the site, such as `/scl/fi/{id}/{name}`.
    path: String,
    /// Its query without `dl` and `raw`: `rlkey` and the like, which are
    /// part of the link's secret.
    query: Vec<(String, String)>,
}

impl DropboxLink {
    /// `link` as a Dropbox shared link, if it is one. Pure.
    pub fn parse(link: &Url) -> Option<Self> {
        if !matches!(link.scheme(), "https" | "http")
            || !HOSTS.iter().any(|h| link.host_str() == Some(h))
        {
            return None;
        }
        let segments: Vec<&str> = link.path_segments()?.filter(|s| !s.is_empty()).collect();
        if segments.iter().any(|s| s.len() > MAX_SEGMENT) {
            return None;
        }
        let id = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        let kind = match segments.as_slice() {
            ["s", key] | ["s", key, _] | ["scl", "fi", key] | ["scl", "fi", key, _] if id(key) => {
                Kind::File
            }
            ["sh", key, ..] | ["scl", "fo", key, ..] if id(key) => Kind::Folder,
            _ => return None,
        };
        let query = link
            .query_pairs()
            .filter(|(name, _)| name != "dl" && name != "raw")
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        Some(Self {
            kind,
            path: format!("/{}", segments.join("/")),
            query,
        })
    }

    /// Where the site serves the file itself, at `site`.
    fn download(&self, site: &Endpoint) -> Url {
        let mut url = site.url(&self.path[1..]);
        url.query_pairs_mut()
            .extend_pairs(&self.query)
            .append_pair("dl", "1");
        url
    }

    /// A stable identity of the link that names no secret: the path and
    /// query carry the link's key.
    fn fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.path.as_bytes());
        for (name, value) in &self.query {
            digest.update([0]);
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(value.as_bytes());
        }
        hex::encode(&digest.finalize()[..16])
    }
}

/// Dropbox's public file links.
pub struct Dropbox {
    inspector: Arc<dyn Inspector>,
    site: Endpoint,
    service: ProviderId,
}

impl std::fmt::Debug for Dropbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dropbox")
            .field("site", &self.site)
            .finish_non_exhaustive()
    }
}

/// What Dropbox answered about a file link.
enum Answer {
    /// The file: its name, its size, and where this answer came from.
    File(Box<Inspection>),
    /// Something else, already explained.
    Refused(Refusal),
}

/// Why Dropbox gave no file.
enum Refusal {
    NotFound,
    Traffic,
    Page,
    Denied,
    Failing(u16),
    Other(u16),
    Unanswered(CheckReason, String),
}

impl Dropbox {
    /// Dropbox's site at `site`, asked through `inspector`.
    pub fn new(inspector: Arc<dyn Inspector>, site: Endpoint) -> Self {
        Self {
            inspector,
            site,
            service: ProviderId::new(SERVICE).expect("a valid service ID"),
        }
    }

    /// The file host, for registering with the engine.
    pub fn host(
        _: Arc<dyn ApiClient>,
        inspector: Arc<dyn Inspector>,
        site: Endpoint,
    ) -> Arc<dyn FileHost> {
        Arc::new(Self::new(inspector, site))
    }

    /// Asks for the file's first byte.
    async fn ask(&self, link: &DropboxLink) -> Answer {
        let request = InspectRequest {
            url: link.download(&self.site),
            method: InspectMethod::FirstByte,
            max_page: MAX_PAGE,
            deadline: DEADLINE,
            // The address is Dropbox's, not the link's author's.
            reach: Reach::Allowed,
        };
        let answer = match self.inspector.inspect(&request).await {
            Ok(answer) => answer,
            Err(e) => return Answer::Refused(Refusal::Unanswered(e.reason, e.message)),
        };
        Answer::Refused(match answer.status {
            200 | 206 if is_page(answer.content_type.as_deref(), answer.attachment) => {
                Refusal::Page
            }
            200 | 206 => return Answer::File(Box::new(answer)),
            404 | 410 => Refusal::NotFound,
            429 => Refusal::Traffic,
            401 | 403 => Refusal::Denied,
            status @ 500.. => Refusal::Failing(status),
            status => Refusal::Other(status),
        })
    }
}

impl Refusal {
    fn check(self) -> HostCheck {
        let unknown = |reason, why: &str| HostCheck::Unknown(reason, why.into());
        match self {
            Self::NotFound => HostCheck::Offline(NOT_FOUND.into()),
            Self::Traffic => unknown(CheckReason::RateLimited, TRAFFIC),
            Self::Page => unknown(CheckReason::AccessDenied, PAGE),
            Self::Denied => unknown(
                CheckReason::AccessDenied,
                "Dropbox refused access to this link.",
            ),
            Self::Failing(status) => HostCheck::Unknown(
                CheckReason::ServerError,
                format!("Dropbox failed to answer (HTTP {status}); check again later."),
            ),
            Self::Other(status) => HostCheck::Unknown(
                CheckReason::Unreadable,
                format!("Dropbox answered HTTP {status}."),
            ),
            Self::Unanswered(reason, why) => HostCheck::Unknown(reason, why),
        }
    }

    fn error(self) -> HostError {
        let busy = |message: String, retry: Duration| HostError {
            code: FailureCode::HostUnavailableViaProvider,
            message,
            retry_after: Some(retry),
        };
        match self {
            Self::NotFound => HostError::new(FailureCode::ResourceDeleted, NOT_FOUND),
            Self::Traffic => busy(TRAFFIC.into(), TRAFFIC_BAN),
            Self::Page => HostError::new(FailureCode::UnsupportedCapability, PAGE),
            Self::Denied => HostError::new(
                FailureCode::PermissionDenied,
                "Dropbox refused access to this file.",
            ),
            Self::Failing(status) => busy(
                format!("Dropbox failed to answer (HTTP {status}). Retry in a minute."),
                BACK_OFF,
            ),
            Self::Other(status) => HostError::new(
                FailureCode::AdapterParseFailure,
                format!("Dropbox answered HTTP {status}."),
            ),
            Self::Unanswered(CheckReason::TimedOut, why) => {
                HostError::new(FailureCode::RequestTimeout, why)
            }
            Self::Unanswered(_, why) => HostError::new(
                FailureCode::HostUnavailableViaProvider,
                if why.is_empty() {
                    "Dropbox could not be reached.".into()
                } else {
                    why
                },
            ),
        }
    }
}

impl FileHost for Dropbox {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn recognizes(&self, link: &Url) -> bool {
        DropboxLink::parse(link).is_some()
    }

    fn check<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, HostCheck> {
        Box::pin(async move {
            match DropboxLink::parse(link) {
                None => HostCheck::Unknown(CheckReason::Unreadable, "Not a Dropbox link.".into()),
                Some(link) if link.kind == Kind::Folder => {
                    HostCheck::Unknown(CheckReason::Unreadable, FOLDER.into())
                }
                Some(link) => match self.ask(&link).await {
                    Answer::File(file) => HostCheck::File {
                        name: file.filename,
                        size: file.length,
                    },
                    Answer::Refused(refusal) => refusal.check(),
                },
            }
        })
    }

    fn resolve<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, Result<HostedFile, HostError>> {
        Box::pin(async move {
            let link = DropboxLink::parse(link).ok_or_else(|| {
                HostError::new(FailureCode::UnsupportedCapability, "Not a Dropbox link.")
            })?;
            if link.kind == Kind::Folder {
                return Err(HostError::new(FailureCode::UnsupportedCapability, FOLDER));
            }
            let file = match self.ask(&link).await {
                Answer::File(file) => file,
                Answer::Refused(refusal) => return Err(refusal.error()),
            };
            let size = file.length.ok_or_else(|| {
                HostError::new(
                    FailureCode::AdapterParseFailure,
                    "Dropbox did not say how large the file is.",
                )
            })?;
            let url = file
                .final_url
                .and_then(common::download_url)
                .ok_or_else(|| {
                    HostError::new(
                        FailureCode::AdapterParseFailure,
                        "Dropbox sent no usable download address.",
                    )
                })?;
            Ok(HostedFile {
                url,
                name: file.filename,
                size,
                identity: format!("{};{size}", link.fingerprint()),
                encoding: None,
                // A link keeps its name when its file is replaced, so only
                // Dropbox's validators tell versions apart; without them
                // nothing saved is continued.
                validators: Some(file.validators),
            })
        })
    }

    fn refused(&self, status: u16) -> Option<HostError> {
        // The temporary URL is refused like the link once Dropbox suspends
        // or removes it mid-download.
        match status {
            404 | 410 => Some(Refusal::NotFound.error()),
            429 => Some(Refusal::Traffic.error()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(link: &str) -> Option<DropboxLink> {
        DropboxLink::parse(&Url::parse(link).unwrap())
    }

    #[test]
    fn file_and_folder_links_are_told_apart() {
        for file in [
            "https://www.dropbox.com/scl/fi/abc123XYZ/report.pdf?rlkey=k1&dl=0",
            "https://www.dropbox.com/scl/fi/abc123XYZ?rlkey=k1",
            "https://www.dropbox.com/s/a1b2c3/report.pdf?dl=0",
            "https://dropbox.com/s/a1b2c3/report.pdf",
            "https://dl.dropboxusercontent.com/s/a1b2c3/report.pdf",
            "https://dl.dropbox.com/scl/fi/abc123XYZ/report.pdf?rlkey=k1&raw=1",
        ] {
            assert_eq!(parse(file).map(|l| l.kind), Some(Kind::File), "{file}");
        }
        for folder in [
            "https://www.dropbox.com/scl/fo/abc123/def456?rlkey=k1&dl=0",
            "https://www.dropbox.com/sh/abc123/def456?dl=0",
            "https://www.dropbox.com/sh/abc123/def456/sub/file.txt",
        ] {
            assert_eq!(
                parse(folder).map(|l| l.kind),
                Some(Kind::Folder),
                "{folder}"
            );
        }
        for other in [
            "https://www.dropbox.com/",
            "https://www.dropbox.com/home/Photos",
            "https://www.dropbox.com/s/",
            "https://www.dropbox.com/s/bad%20id/x.pdf",
            "https://www.dropbox.com/scl/fi/abc/name/extra",
            "https://www.dropbox.com.evil.example/s/a1b2c3/x.pdf",
            "https://paper.dropbox.com/s/a1b2c3/x.pdf",
            "ftp://www.dropbox.com/s/a1b2c3/x.pdf",
        ] {
            assert_eq!(parse(other), None, "{other}");
        }
    }

    #[test]
    fn the_download_keeps_the_key_and_switches_dl_on() {
        let site = Endpoint::official(OFFICIAL_SITE);
        let link =
            parse("https://dl.dropbox.com/scl/fi/abc123/a%20b.zip?rlkey=k1&raw=1&st=s2&dl=0")
                .unwrap();
        assert_eq!(
            link.download(&site).as_str(),
            "https://www.dropbox.com/scl/fi/abc123/a%20b.zip?rlkey=k1&st=s2&dl=1"
        );
        let fixture = Endpoint::custom("http://127.0.0.1:9/box/").unwrap();
        assert_eq!(
            link.download(&fixture).as_str(),
            "http://127.0.0.1:9/box/scl/fi/abc123/a%20b.zip?rlkey=k1&st=s2&dl=1"
        );
    }

    #[test]
    fn the_identity_ignores_the_switches_and_hides_the_key() {
        let a = parse("https://www.dropbox.com/scl/fi/abc/x.zip?rlkey=k1&dl=0").unwrap();
        let b = parse("https://dl.dropboxusercontent.com/scl/fi/abc/x.zip?rlkey=k1").unwrap();
        let other = parse("https://www.dropbox.com/scl/fi/abc/x.zip?rlkey=k2").unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), other.fingerprint());
        assert!(!a.fingerprint().contains("k1"));
    }
}
