//! Pixeldrain: public file and list links, through Pixeldrain's API.
//!
//! A file's details (name, size, SHA-256) come from its `info`, which is
//! not counted as a download, and its bytes from `api/file/{id}`, which
//! accepts ranges. A file ID never names other bytes, so saved bytes are
//! continued under the ID, and the finished file is checked against the
//! SHA-256. A list (album) is listed onto the Links list as its files. No
//! account or API key is used: Pixeldrain may then ask for a captcha on its
//! site before a file can be downloaded, which Wayfetch only reports.
//! Filesystem shares (`/d/`) are not supported yet.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use wayfetch_engine::accounts::provider::{ApiClient, ApiMethod, ApiResponse, ProviderError};
use wayfetch_engine::crawl::Inspector;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::name::ProviderId;
use wayfetch_engine::hosting::{
    Decoder, Encoding, FileHost, HostCheck, HostError, HostedFile, MAX_FOLDER_FILES, Verifier,
};
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::common::{self, Api, Endpoint};

/// The service's ID, as in the UI's catalogue.
pub const SERVICE: &str = "pixeldrain";
/// Pixeldrain's API.
pub const OFFICIAL_API: &str = "https://pixeldrain.com/api/";
/// Where links to a list's files point.
const SITE: &str = "https://pixeldrain.com/";
/// Hosts whose paths are shared links.
const HOSTS: &[&str] = &[
    "pixeldrain.com",
    "www.pixeldrain.com",
    "pixeldrain.net",
    "www.pixeldrain.net",
    "pixeldra.in",
];
/// Longest file or list ID accepted.
const MAX_ID: usize = 64;
/// Largest API answer read: a list's whole listing comes in one.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// How long to wait after Pixeldrain is busy or asks to slow down.
const BACK_OFF: Duration = Duration::from_secs(60);
/// How long to wait once the free daily limit is used up.
const DAILY_LIMIT: Duration = Duration::from_secs(4 * 60 * 60);

const NOT_FOUND: &str = "Pixeldrain says this does not exist or was removed.";
const TAKEN_DOWN: &str = "Pixeldrain took this file down after a report and no longer serves it.";
const CAPTCHA: &str = "Pixeldrain asks for a captcha before this file can be downloaded, as it \
     does for download managers without a premium account. Open the file on pixeldrain.com and \
     solve it there, then retry the download.";
const LIMIT: &str = "Pixeldrain's free download limit for this connection is used up. It refills \
     over 24 hours; retry the download later.";
const BUSY: &str = "Pixeldrain is busy or asked Wayfetch to slow down. Retry in a minute.";
const SHARE: &str = "This is a Pixeldrain filesystem share. Wayfetch downloads Pixeldrain files \
     and lists for now; add a link to each file instead.";
const LIST: &str = "This is a Pixeldrain list. Its files are listed on the Links list when it \
     is checked; start them from there.";

/// Which kind of thing a link names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One file.
    File,
    /// A list (album) of files.
    List,
    /// A filesystem share.
    Share,
}

/// A Pixeldrain link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixeldrainLink {
    /// What it names.
    pub kind: Kind,
    /// The file's, list's, or share's ID.
    pub id: String,
}

impl PixeldrainLink {
    /// `link` as a Pixeldrain link, if it is one. Pure.
    pub fn parse(link: &Url) -> Option<Self> {
        if !matches!(link.scheme(), "https" | "http")
            || !HOSTS.iter().any(|h| link.host_str() == Some(h))
        {
            return None;
        }
        let segments: Vec<&str> = link.path_segments()?.filter(|s| !s.is_empty()).collect();
        let (kind, id) = match segments.as_slice() {
            ["u", id] | ["api", "file", id] | ["api", "file", id, "info"] => (Kind::File, *id),
            ["l", id] | ["api", "list", id] => (Kind::List, *id),
            ["d", id, ..] | ["api", "filesystem", id, ..] => (Kind::Share, *id),
            _ => return None,
        };
        let valid = !id.is_empty()
            && id.len() <= MAX_ID
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        valid.then(|| Self {
            kind,
            id: id.to_owned(),
        })
    }

    /// The link to file `id` on Pixeldrain's site.
    pub fn file(id: &str) -> Url {
        Url::parse(SITE)
            .and_then(|site| site.join(&format!("u/{id}")))
            .expect("a valid file link")
    }
}

/// A file as Pixeldrain describes it.
#[derive(Deserialize)]
struct FileInfo {
    #[serde(default)]
    name: Option<String>,
    size: u64,
    /// The SHA-256 of its content, in hex.
    #[serde(default)]
    hash_sha256: Option<String>,
    /// Why it cannot be downloaded right now, such as a captcha; empty when
    /// it can.
    #[serde(default)]
    availability: Option<String>,
    #[serde(default = "yes")]
    can_download: bool,
}

fn yes() -> bool {
    true
}

/// A list as Pixeldrain describes it.
#[derive(Deserialize)]
struct ListInfo {
    #[serde(default)]
    files: Vec<ListFile>,
}

#[derive(Deserialize)]
struct ListFile {
    #[serde(default)]
    id: Option<String>,
}

/// An error answer.
#[derive(Deserialize)]
struct ErrorInfo {
    #[serde(default)]
    value: String,
}

/// Why Pixeldrain gave no file.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    NotFound,
    TakenDown,
    Captcha,
    Limit,
    Busy,
    Denied,
    Other(u16),
    Unanswered(String),
}

impl Refusal {
    /// What Pixeldrain's error answer means.
    fn of(response: &ApiResponse) -> Self {
        let value = serde_json::from_slice::<ErrorInfo>(response.body.expose())
            .map(|e| e.value)
            .unwrap_or_default();
        match response.status {
            451 => Self::TakenDown,
            404 | 410 => Self::NotFound,
            status => Self::of_value(&value).unwrap_or(match status {
                429 | 500.. => Self::Busy,
                401 | 403 => Self::Denied,
                status => Self::Other(status),
            }),
        }
    }

    /// What one of Pixeldrain's error codes means, if it is one this
    /// integration tells apart. A file's `availability` carries the same
    /// codes as an error answer.
    fn of_value(value: &str) -> Option<Self> {
        Some(match value {
            "unavailable_for_legal_reasons" => Self::TakenDown,
            "not_found" => Self::NotFound,
            v if v.ends_with("_captcha_required") || v == "hotlink_detected" => Self::Captcha,
            "transfer_limit_exceeded" | "download_limit_exceeded" => Self::Limit,
            "max_concurrent_downloads" | "ip_rate_limit_reached" => Self::Busy,
            _ => return None,
        })
    }

    /// What a file's `availability` means, if anything stops its download.
    fn of_availability(info: &FileInfo) -> Option<Self> {
        match info.availability.as_deref().unwrap_or_default() {
            "" if info.can_download => None,
            "" => Some(Self::Denied),
            value => Some(Self::of_value(value).unwrap_or(Self::Denied)),
        }
    }

    fn check(self) -> HostCheck {
        let unknown = |reason, why: &str| HostCheck::Unknown(reason, why.into());
        match self {
            Self::NotFound => HostCheck::Offline(NOT_FOUND.into()),
            Self::TakenDown => HostCheck::Offline(TAKEN_DOWN.into()),
            Self::Captcha => unknown(CheckReason::AccessDenied, CAPTCHA),
            Self::Limit => unknown(CheckReason::RateLimited, LIMIT),
            Self::Busy => unknown(CheckReason::RateLimited, BUSY),
            Self::Denied => unknown(
                CheckReason::AccessDenied,
                "Pixeldrain refused access to this link.",
            ),
            Self::Other(status) => HostCheck::Unknown(
                CheckReason::Unreadable,
                format!("Pixeldrain answered HTTP {status}."),
            ),
            Self::Unanswered(why) => HostCheck::Unknown(CheckReason::Unreachable, why),
        }
    }

    fn error(self) -> HostError {
        let wait = |message: &str, retry: Duration| HostError {
            code: FailureCode::HostUnavailableViaProvider,
            message: message.into(),
            retry_after: Some(retry),
        };
        match self {
            Self::NotFound => HostError::new(FailureCode::ResourceDeleted, NOT_FOUND),
            Self::TakenDown => HostError::new(FailureCode::ResourceDeleted, TAKEN_DOWN),
            Self::Captcha => HostError::new(FailureCode::PermissionDenied, CAPTCHA),
            Self::Limit => wait(LIMIT, DAILY_LIMIT),
            Self::Busy => wait(BUSY, BACK_OFF),
            Self::Denied => HostError::new(
                FailureCode::PermissionDenied,
                "Pixeldrain refused access to this file.",
            ),
            Self::Other(status) => HostError::new(
                FailureCode::AdapterParseFailure,
                format!("Pixeldrain answered HTTP {status}."),
            ),
            Self::Unanswered(why) => HostError::new(FailureCode::HostUnavailableViaProvider, why),
        }
    }
}

impl From<ProviderError> for Refusal {
    fn from(e: ProviderError) -> Self {
        Self::Unanswered(e.to_string())
    }
}

/// Pixeldrain's public links.
pub struct Pixeldrain {
    api: Api,
    service: ProviderId,
}

impl std::fmt::Debug for Pixeldrain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pixeldrain")
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl Pixeldrain {
    /// Pixeldrain's API at `endpoint`, through `client`.
    pub fn new(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Self {
        Self {
            api: Api::new(client, "Pixeldrain", endpoint).with_max_body(MAX_BODY),
            service: ProviderId::new(SERVICE).expect("a valid service ID"),
        }
    }

    /// The file host, for registering with the engine.
    pub fn host(
        client: Arc<dyn ApiClient>,
        _: Arc<dyn Inspector>,
        endpoint: Endpoint,
    ) -> Arc<dyn FileHost> {
        Arc::new(Self::new(client, endpoint))
    }

    /// The JSON answer at `path`.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        what: &str,
    ) -> Result<T, Refusal> {
        let url = self.api.endpoint().url(path);
        let response = self.api.call(ApiMethod::Get, url, None, None).await?;
        if response.status != 200 {
            return Err(Refusal::of(&response));
        }
        Ok(self.api.parse(&response, what)?)
    }

    async fn info(&self, id: &str) -> Result<FileInfo, Refusal> {
        self.get(&format!("file/{id}/info"), "a file's details")
            .await
    }

    /// Links to the list's files, at most [`MAX_FOLDER_FILES`].
    async fn list(&self, id: &str) -> Result<Vec<Url>, Refusal> {
        let list: ListInfo = self.get(&format!("list/{id}"), "a list").await?;
        Ok(list
            .files
            .into_iter()
            .filter_map(|f| f.id)
            .filter_map(|id| {
                let link = PixeldrainLink::file(&id);
                PixeldrainLink::parse(&link).map(|_| link)
            })
            .take(MAX_FOLDER_FILES)
            .collect())
    }
}

/// The SHA-256 Pixeldrain gave, if any: `Err` if it is not one, which
/// must not quietly turn the check off.
fn sha256(value: Option<&str>) -> Result<Option<[u8; 32]>, HostError> {
    match value {
        None | Some("") => Ok(None),
        Some(hash) => hex::decode(hash)
            .ok()
            .and_then(|digest| digest.try_into().ok())
            .map(Some)
            .ok_or_else(|| {
                HostError::new(
                    FailureCode::AdapterParseFailure,
                    "Pixeldrain gave a malformed SHA-256 for this file.",
                )
            }),
    }
}

impl FileHost for Pixeldrain {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn recognizes(&self, link: &Url) -> bool {
        PixeldrainLink::parse(link).is_some()
    }

    fn check<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, HostCheck> {
        Box::pin(async move {
            let Some(link) = PixeldrainLink::parse(link) else {
                return HostCheck::Unknown(
                    CheckReason::Unreadable,
                    "Not a Pixeldrain link.".into(),
                );
            };
            match link.kind {
                Kind::Share => HostCheck::Unknown(CheckReason::Unreadable, SHARE.into()),
                Kind::List => match self.list(&link.id).await {
                    Ok(files) => HostCheck::Folder(files),
                    Err(refusal) => refusal.check(),
                },
                Kind::File => match self.info(&link.id).await {
                    Ok(info) => match Refusal::of_availability(&info) {
                        Some(refusal) => refusal.check(),
                        None => HostCheck::File {
                            name: info.name,
                            size: Some(info.size),
                        },
                    },
                    Err(refusal) => refusal.check(),
                },
            }
        })
    }

    fn resolve<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, Result<HostedFile, HostError>> {
        Box::pin(async move {
            let link = PixeldrainLink::parse(link).ok_or_else(|| {
                HostError::new(FailureCode::UnsupportedCapability, "Not a Pixeldrain link.")
            })?;
            match link.kind {
                Kind::File => {}
                Kind::List => return Err(HostError::new(FailureCode::UnsupportedCapability, LIST)),
                Kind::Share => {
                    return Err(HostError::new(FailureCode::UnsupportedCapability, SHARE));
                }
            }
            let info = self.info(&link.id).await.map_err(Refusal::error)?;
            if let Some(refusal) = Refusal::of_availability(&info) {
                return Err(refusal.error());
            }
            let url = common::download_url(self.api.endpoint().url(&format!("file/{}", link.id)))
                .ok_or_else(|| {
                HostError::new(
                    FailureCode::AdapterParseFailure,
                    "Pixeldrain's download address is not usable.",
                )
            })?;
            let digest = sha256(info.hash_sha256.as_deref())?;
            Ok(HostedFile {
                url,
                name: info.name,
                size: info.size,
                identity: match digest {
                    Some(digest) => format!("{};{};{}", link.id, info.size, hex::encode(digest)),
                    None => format!("{};{}", link.id, info.size),
                },
                encoding: digest.map(|d| Arc::new(Checked(d)) as Arc<dyn Encoding>),
                // A file ID never names other bytes.
                validators: None,
            })
        })
    }

    fn refused(&self, status: u16) -> Option<HostError> {
        // Pixeldrain refuses the bytes as it refuses the details once a
        // captcha or a limit applies mid-download; the body that would say
        // which is not seen here.
        match status {
            404 | 410 => Some(Refusal::NotFound.error()),
            451 => Some(Refusal::TakenDown.error()),
            403 => Some(HostError::new(
                FailureCode::PermissionDenied,
                "Pixeldrain refused the download: it may ask for a captcha on pixeldrain.com, or \
                 this connection's free limit is used up. Open the file there, then retry.",
            )),
            429 => Some(Refusal::Busy.error()),
            _ => None,
        }
    }
}

/// A file stored as it is, checked against its SHA-256.
struct Checked([u8; 32]);

impl Encoding for Checked {
    fn decoder(&self, _: u64) -> Box<dyn Decoder> {
        Box::new(AsStored)
    }

    fn verifier(&self) -> Box<dyn Verifier> {
        Box::new(Sha256Check {
            expected: self.0,
            digest: Sha256::new(),
        })
    }
}

/// Leaves bytes as they are.
struct AsStored;

impl Decoder for AsStored {
    fn decode(&mut self, _: &mut [u8]) {}
}

struct Sha256Check {
    expected: [u8; 32],
    digest: Sha256,
}

impl Verifier for Sha256Check {
    fn update(&mut self, data: &[u8]) {
        self.digest.update(data);
    }

    fn verify(self: Box<Self>) -> bool {
        self.digest.finalize()[..] == self.expected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(link: &str) -> Option<PixeldrainLink> {
        PixeldrainLink::parse(&Url::parse(link).unwrap())
    }

    #[test]
    fn files_lists_and_shares_are_told_apart() {
        for (link, kind) in [
            ("https://pixeldrain.com/u/abc123XY", Kind::File),
            ("https://pixeldrain.com/u/abc123XY?embed", Kind::File),
            ("https://www.pixeldrain.com/u/abc123XY/", Kind::File),
            ("https://pixeldra.in/u/abc123XY", Kind::File),
            ("https://pixeldrain.net/u/abc123XY", Kind::File),
            ("https://pixeldrain.com/api/file/abc123XY", Kind::File),
            ("https://pixeldrain.com/api/file/abc123XY/info", Kind::File),
            ("https://pixeldrain.com/l/L1st_-9a#item=2", Kind::List),
            ("https://pixeldrain.com/api/list/L1st_-9a", Kind::List),
            (
                "https://pixeldrain.com/d/abcd1234/sub/file.txt",
                Kind::Share,
            ),
        ] {
            let parsed = parse(link).unwrap_or_else(|| panic!("{link}"));
            assert_eq!(parsed.kind, kind, "{link}");
        }
        assert_eq!(
            parse("https://pixeldrain.com/l/L1st_-9a").unwrap().id,
            "L1st_-9a"
        );
        for other in [
            "https://pixeldrain.com/",
            "https://pixeldrain.com/u/",
            "https://pixeldrain.com/u/a,b",
            "https://pixeldrain.com/u/abc/extra",
            "https://pixeldrain.com/api/file/abc/thumbnail",
            "https://pixeldrain.com/user/files",
            "https://pixeldrain.com.evil.example/u/abc123XY",
            "https://cdn.pixeldrain.example/u/abc123XY",
            "ftp://pixeldrain.com/u/abc123XY",
        ] {
            assert_eq!(parse(other), None, "{other}");
        }
        assert_eq!(
            parse(&format!(
                "https://pixeldrain.com/u/{}",
                "a".repeat(MAX_ID + 1)
            )),
            None
        );
    }

    #[test]
    fn a_lists_files_link_to_the_site() {
        let link = PixeldrainLink::file("abc123XY");
        assert_eq!(link.as_str(), "https://pixeldrain.com/u/abc123XY");
        assert_eq!(PixeldrainLink::parse(&link).unwrap().kind, Kind::File);
    }

    #[test]
    fn error_values_have_their_meaning() {
        let of = |status, body: &str| {
            Refusal::of(&ApiResponse {
                status,
                body: wayfetch_engine::accounts::Secret::from_text(body),
            })
        };
        let error = |value: &str| format!(r#"{{"success":false,"value":"{value}"}}"#);
        assert_eq!(of(404, &error("not_found")), Refusal::NotFound);
        assert_eq!(
            of(451, &error("unavailable_for_legal_reasons")),
            Refusal::TakenDown
        );
        for captcha in [
            "file_rate_limited_captcha_required",
            "virus_detected_captcha_required",
            "ip_download_limited_captcha_required",
            "server_overload_captcha_required",
            "hotlink_detected",
        ] {
            assert_eq!(of(403, &error(captcha)), Refusal::Captcha, "{captcha}");
        }
        assert_eq!(of(403, &error("transfer_limit_exceeded")), Refusal::Limit);
        assert_eq!(of(403, &error("max_concurrent_downloads")), Refusal::Busy);
        assert_eq!(of(429, &error("ip_rate_limit_reached")), Refusal::Busy);
        assert_eq!(of(502, "<html>"), Refusal::Busy);
        assert_eq!(of(403, &error("forbidden")), Refusal::Denied);
        assert_eq!(of(400, "nonsense"), Refusal::Other(400));
    }

    #[test]
    fn availability_carries_the_same_codes() {
        let of = |availability: &str, can_download| {
            Refusal::of_availability(&FileInfo {
                name: None,
                size: 1,
                hash_sha256: None,
                availability: Some(availability.into()),
                can_download,
            })
        };
        assert_eq!(of("", true), None);
        assert_eq!(of("", false), Some(Refusal::Denied));
        assert_eq!(
            of("file_rate_limited_captcha_required", true),
            Some(Refusal::Captcha)
        );
        assert_eq!(of("transfer_limit_exceeded", true), Some(Refusal::Limit));
        assert_eq!(
            of("unavailable_for_legal_reasons", true),
            Some(Refusal::TakenDown)
        );
        assert_eq!(of("max_concurrent_downloads", true), Some(Refusal::Busy));
        assert_eq!(of("something_new", true), Some(Refusal::Denied));
    }

    #[test]
    fn the_sha256_check_passes_only_the_stored_file() {
        let data = b"pixeldrain".to_vec();
        let encoding = Checked(Sha256::digest(&data).into());
        let mut chunk = data.clone();
        encoding.decoder(3).decode(&mut chunk);
        assert_eq!(chunk, data, "bytes are stored as they are");
        let mut check = encoding.verifier();
        check.update(&data[..4]);
        check.update(&data[4..]);
        assert!(check.verify());
        let mut check = encoding.verifier();
        check.update(b"pixeldraim");
        assert!(!check.verify());
        let hash = hex::encode(Sha256::digest(&data));
        assert_eq!(sha256(Some(&hash)), Ok(Some(encoding.0)));
        assert_eq!(sha256(Some(&hash.to_uppercase())), Ok(Some(encoding.0)));
        for malformed in ["abc123", "00", "zz", &hash[1..], &format!("{hash}00")] {
            assert_eq!(
                sha256(Some(malformed)).map_err(|e| e.code),
                Err(FailureCode::AdapterParseFailure),
                "{malformed}"
            );
        }
        assert_eq!(sha256(None), Ok(None));
        assert_eq!(sha256(Some("")), Ok(None));
    }
}
