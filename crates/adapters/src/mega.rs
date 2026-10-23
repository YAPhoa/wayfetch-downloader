//! MEGA: public file and folder links, decrypted on this machine.
//!
//! MEGA encrypts files before storing them, and a shared link carries the
//! key in its fragment. The key is used here only: requests name the file
//! by its handle, and the engine downloads the stored bytes from the URL
//! MEGA gives, decrypting them as they are written ([`crypto`]) and checking
//! MEGA's MAC before the file is published. No account is used.

pub mod crypto;
pub mod link;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use url::Url;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{ApiClient, ProviderError};
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::name::ProviderId;
use wayfetch_engine::hosting::{
    Decoder, Encoding, FileHost, HostCheck, HostError, HostedFile, MAX_FOLDER_FILES, Verifier,
};
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::common::{Api, Endpoint, now_millis};
use crypto::{FileKey, MacCheck};
use link::{FolderKey, Handle, Inner, MegaLink};

/// The service's ID, as in the UI's catalogue.
pub const SERVICE: &str = "mega";
/// MEGA's API.
pub const OFFICIAL_API: &str = "https://g.api.mega.co.nz/";
/// Largest API answer read: a folder's whole listing comes in one.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Longest a download URL MEGA gives may be.
const MAX_URL: usize = 4096;
/// How long to wait after MEGA asks to slow down.
const BACK_OFF: Duration = Duration::from_secs(60);

/// MEGA's error numbers this integration tells apart.
mod code {
    /// Try again.
    pub const AGAIN: i64 = -3;
    /// Too many requests.
    pub const RATE_LIMIT: i64 = -4;
    /// Too many concurrent requests.
    pub const TOO_MANY: i64 = -6;
    /// Not found.
    pub const NOT_FOUND: i64 = -9;
    /// Access denied.
    pub const ACCESS: i64 = -11;
    /// Taken down.
    pub const BLOCKED: i64 = -16;
    /// Transfer quota used up.
    pub const OVER_QUOTA: i64 = -17;
    /// Temporarily unavailable.
    pub const UNAVAILABLE: i64 = -18;
    /// Transfer quota about to be used up.
    pub const GOING_OVER_QUOTA: i64 = -24;
}

const QUOTA: &str = "MEGA's free transfer quota for this connection is used up. MEGA refills it \
     over a few hours; retry the download later.";

/// MEGA's public links.
pub struct Mega {
    api: Api,
    service: ProviderId,
    sequence: AtomicU64,
}

impl std::fmt::Debug for Mega {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mega")
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

/// A file as MEGA describes it.
#[derive(Deserialize)]
struct FileInfo {
    /// Its size.
    s: u64,
    /// Its encrypted attributes.
    #[serde(default)]
    at: Option<String>,
    /// A download URL, when asked for.
    #[serde(default)]
    g: Option<Value>,
}

/// A folder's listing.
#[derive(Deserialize)]
struct Listing {
    f: Vec<Node>,
}

/// One node of a folder's listing.
#[derive(Deserialize)]
struct Node {
    /// Its handle.
    h: String,
    /// Its parent's handle.
    #[serde(default)]
    p: Option<String>,
    /// 0 for a file, 1 for a folder.
    t: u8,
    /// Its key, encrypted: `owner:key` pairs separated by `/`.
    #[serde(default)]
    k: Option<String>,
}

/// What MEGA answered a command with.
enum Answer<T> {
    Ok(T),
    Error(i64),
}

impl Mega {
    /// MEGA at `endpoint`, through `client`.
    pub fn new(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Self {
        Self {
            api: Api::new(client, "MEGA", endpoint).with_max_body(MAX_BODY),
            service: ProviderId::new(SERVICE).expect("a valid service ID"),
            // Request IDs only need to differ between requests.
            sequence: AtomicU64::new(now_millis().unsigned_abs()),
        }
    }

    /// The file host, for registering with the engine.
    pub fn host(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Arc<dyn FileHost> {
        Arc::new(Self::new(client, endpoint))
    }

    /// Sends one command, in the context of the shared folder `folder` if
    /// given.
    async fn command<T: for<'de> Deserialize<'de>>(
        &self,
        command: Value,
        folder: Option<&Handle>,
        what: &str,
    ) -> Result<Answer<T>, ProviderError> {
        let mut url = self.api.endpoint().url("cs");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair(
                "id",
                &self.sequence.fetch_add(1, Ordering::Relaxed).to_string(),
            );
            if let Some(folder) = folder {
                query.append_pair("n", folder.as_str());
            }
        }
        let body = Secret::new(serde_json::to_vec(&[command]).expect("plain data serializes"));
        let response = self.api.post_json(url, body).await?;
        if response.status != 200 {
            return Err(self.api.unexpected(&response, what, ""));
        }
        // A request-wide error is a bare number; otherwise one answer per
        // command, a number for an error.
        match self.api.parse::<Value>(&response, what)? {
            Value::Number(n) => Ok(Answer::Error(n.as_i64().unwrap_or(-1))),
            Value::Array(mut answers) if answers.len() == 1 => match answers.remove(0) {
                Value::Number(n) => Ok(Answer::Error(n.as_i64().unwrap_or(-1))),
                answer => serde_json::from_value(answer)
                    .map(Answer::Ok)
                    .map_err(|_| self.api.malformed(what)),
            },
            _ => Err(self.api.malformed(what)),
        }
    }

    /// What MEGA says about the file `node` of `link`, with a download URL
    /// if `download`.
    async fn file(
        &self,
        link: &MegaLink,
        download: bool,
    ) -> Result<Answer<(FileInfo, FileKey, String)>, Lookup> {
        let (command, folder, key, identity) = match link {
            MegaLink::File { handle, key } => (
                serde_json::json!({ "a": "g", "p": handle.as_str() }),
                None,
                key.clone(),
                handle.as_str().to_owned(),
            ),
            MegaLink::Folder {
                handle,
                key,
                node: Some(Inner::File(node)),
            } => {
                let key = self.folder_file_key(handle, key, node).await?;
                let key = match key {
                    Answer::Ok(key) => key,
                    Answer::Error(e) => return Ok(Answer::Error(e)),
                };
                (
                    serde_json::json!({ "a": "g", "n": node.as_str() }),
                    Some(handle),
                    key,
                    format!("{}/{}", handle.as_str(), node.as_str()),
                )
            }
            MegaLink::Folder { .. } => return Err(Lookup::Folder),
        };
        let mut command = command;
        if download {
            command["g"] = 1.into();
            command["ssl"] = 2.into();
        }
        let answer = self
            .command::<FileInfo>(command, folder, "a file request")
            .await?;
        Ok(match answer {
            Answer::Ok(info) => Answer::Ok((info, key, identity)),
            Answer::Error(e) => Answer::Error(e),
        })
    }

    /// The key of the file `node` in the shared folder `folder`, from the
    /// folder's listing.
    async fn folder_file_key(
        &self,
        folder: &Handle,
        key: &FolderKey,
        node: &Handle,
    ) -> Result<Answer<FileKey>, ProviderError> {
        let listing = match self.listing(folder).await? {
            Answer::Ok(listing) => listing,
            Answer::Error(e) => return Ok(Answer::Error(e)),
        };
        listing
            .f
            .iter()
            .find(|n| n.t == 0 && n.h == node.as_str())
            .map_or(Ok(Answer::Error(code::NOT_FOUND)), |n| {
                node_key(n, folder, key)
                    .map(Answer::Ok)
                    .ok_or_else(|| self.api.malformed("a folder listing"))
            })
    }

    async fn listing(&self, folder: &Handle) -> Result<Answer<Listing>, ProviderError> {
        let command = serde_json::json!({ "a": "f", "c": 1, "ca": 1, "r": 1 });
        self.command(command, Some(folder), "a folder listing")
            .await
    }

    /// Links to the files below `start` (the shared folder itself if
    /// `None`), flattened, at most [`MAX_FOLDER_FILES`].
    async fn folder_files(
        &self,
        folder: &Handle,
        key: &FolderKey,
        start: Option<&Handle>,
    ) -> Result<Answer<Vec<Url>>, ProviderError> {
        let listing = match self.listing(folder).await? {
            Answer::Ok(listing) => listing,
            Answer::Error(e) => return Ok(Answer::Error(e)),
        };
        let root = start.unwrap_or(folder).as_str();
        let mut children: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
        for node in &listing.f {
            if let Some(parent) = &node.p {
                children.entry(parent.as_str()).or_default().push(node);
            }
        }
        if start.is_some() && !listing.f.iter().any(|n| n.t == 1 && n.h == root) {
            return Ok(Answer::Error(code::NOT_FOUND));
        }
        // Breadth first, each folder once, so a malformed listing cannot
        // loop.
        let mut files = Vec::new();
        let mut seen = BTreeSet::from([root]);
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(parent) = queue.pop_front() {
            for node in children.get(parent).into_iter().flatten() {
                match node.t {
                    0 if files.len() < MAX_FOLDER_FILES => {
                        if let Some(handle) = Handle::new(&node.h) {
                            files.push(MegaLink::in_folder(folder, key, &handle));
                        }
                    }
                    1 if seen.insert(node.h.as_str()) => queue.push_back(node.h.as_str()),
                    _ => {}
                }
            }
        }
        Ok(Answer::Ok(files))
    }
}

/// Why a link has no single file.
enum Lookup {
    /// It names a folder.
    Folder,
    /// MEGA could not be asked.
    Provider(ProviderError),
}

impl From<ProviderError> for Lookup {
    fn from(e: ProviderError) -> Self {
        Self::Provider(e)
    }
}

/// A listed file's key, decrypted with the shared folder's key. The key
/// paired with the folder's handle is used, else the only one.
fn node_key(node: &Node, folder: &Handle, key: &FolderKey) -> Option<FileKey> {
    let pairs: Vec<(&str, &str)> = node
        .k
        .as_deref()?
        .split('/')
        .filter_map(|pair| pair.split_once(':'))
        .collect();
    let encrypted = pairs
        .iter()
        .find(|(owner, _)| *owner == folder.as_str())
        .or(pairs.first().filter(|_| pairs.len() == 1))?
        .1;
    FileKey::in_folder(encrypted, key)
}

/// A download URL MEGA gave: HTTPS (or plain HTTP to a loopback test
/// server), no credentials, bounded.
fn download_url(value: Option<&Value>) -> Option<Url> {
    let url = Url::parse(value?.as_str()?).ok()?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    let ok = (url.scheme() == "https" || url.scheme() == "http" && loopback)
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.as_str().len() <= MAX_URL;
    ok.then_some(url)
}

/// What MEGA's error number means for a download.
fn refusal(error: i64) -> HostError {
    match error {
        code::NOT_FOUND => HostError::new(
            FailureCode::ResourceDeleted,
            "MEGA says this file does not exist or was removed.",
        ),
        code::BLOCKED => HostError::new(
            FailureCode::ResourceDeleted,
            "MEGA took this file down and no longer serves it.",
        ),
        code::OVER_QUOTA | code::GOING_OVER_QUOTA => {
            HostError::new(FailureCode::QuotaExhausted, QUOTA)
        }
        code::AGAIN | code::RATE_LIMIT | code::TOO_MANY | code::UNAVAILABLE => HostError {
            code: FailureCode::HostUnavailableViaProvider,
            message: "MEGA is busy or asked Wayfetch to slow down. Retry in a minute.".into(),
            retry_after: Some(BACK_OFF),
        },
        code::ACCESS => HostError::new(
            FailureCode::HostUnavailableViaProvider,
            "MEGA refused access to this file.",
        ),
        other => HostError::new(
            FailureCode::AdapterParseFailure,
            format!("MEGA refused the request (error {other})."),
        ),
    }
}

/// What MEGA's error number means for a link check.
fn check_refusal(error: i64) -> HostCheck {
    match error {
        code::NOT_FOUND => {
            HostCheck::Offline("MEGA says this does not exist or was removed.".into())
        }
        code::BLOCKED => HostCheck::Offline("MEGA took this down.".into()),
        code::AGAIN | code::RATE_LIMIT | code::TOO_MANY | code::UNAVAILABLE => HostCheck::Unknown(
            CheckReason::RateLimited,
            "MEGA is busy or asked Wayfetch to slow down; check again in a minute.".into(),
        ),
        code::ACCESS => HostCheck::Unknown(
            CheckReason::AccessDenied,
            "MEGA refused access to this link.".into(),
        ),
        other => HostCheck::Unknown(
            CheckReason::Unreadable,
            format!("MEGA refused the check (error {other})."),
        ),
    }
}

fn unavailable(e: &ProviderError) -> HostError {
    HostError::new(FailureCode::HostUnavailableViaProvider, e.to_string())
}

const FOLDER: &str = "This is a MEGA folder. Its files are listed on the Links list when it is \
     checked; start them from there.";

impl FileHost for Mega {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn recognizes(&self, link: &Url) -> bool {
        MegaLink::parse(link).is_some()
    }

    fn check<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, HostCheck> {
        Box::pin(async move {
            let Some(parsed) = MegaLink::parse(link) else {
                return HostCheck::Unknown(CheckReason::Unreadable, "Not a MEGA link.".into());
            };
            if let MegaLink::Folder {
                handle,
                key,
                node: None | Some(Inner::Folder(_)),
            } = &parsed
            {
                let start = match &parsed {
                    MegaLink::Folder {
                        node: Some(Inner::Folder(sub)),
                        ..
                    } => Some(sub),
                    _ => None,
                };
                return match self.folder_files(handle, key, start).await {
                    Ok(Answer::Ok(files)) => HostCheck::Folder(files),
                    Ok(Answer::Error(e)) => check_refusal(e),
                    Err(e) => HostCheck::Unknown(CheckReason::Unreachable, e.to_string()),
                };
            }
            match self.file(&parsed, false).await {
                Ok(Answer::Ok((info, key, _))) => HostCheck::File {
                    name: info.at.as_deref().and_then(|at| key.name(at)),
                    size: Some(info.s),
                },
                Ok(Answer::Error(e)) => check_refusal(e),
                Err(Lookup::Provider(e)) => {
                    HostCheck::Unknown(CheckReason::Unreachable, e.to_string())
                }
                Err(Lookup::Folder) => HostCheck::Unknown(CheckReason::Unreadable, FOLDER.into()),
            }
        })
    }

    fn resolve<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, Result<HostedFile, HostError>> {
        Box::pin(async move {
            let parsed = MegaLink::parse(link).ok_or_else(|| {
                HostError::new(FailureCode::UnsupportedCapability, "Not a MEGA link.")
            })?;
            let (info, key, handle) = match self.file(&parsed, true).await {
                Ok(Answer::Ok(found)) => found,
                Ok(Answer::Error(e)) => return Err(refusal(e)),
                Err(Lookup::Folder) => {
                    return Err(HostError::new(FailureCode::UnsupportedCapability, FOLDER));
                }
                Err(Lookup::Provider(e)) => return Err(unavailable(&e)),
            };
            let url = match &info.g {
                Some(Value::Number(n)) => return Err(refusal(n.as_i64().unwrap_or(-1))),
                other => download_url(other.as_ref()).ok_or_else(|| {
                    HostError::new(
                        FailureCode::AdapterParseFailure,
                        "MEGA sent no usable download address.",
                    )
                })?,
            };
            Ok(HostedFile {
                url,
                name: info.at.as_deref().and_then(|at| key.name(at)),
                size: info.s,
                identity: format!("{handle};{};{}", info.s, key.fingerprint()),
                encoding: Some(Arc::new(Encrypted(key))),
            })
        })
    }

    fn refused(&self, status: u16) -> Option<HostError> {
        // MEGA's storage servers answer 509 once the quota is used up.
        (status == 509).then(|| HostError::new(FailureCode::QuotaExhausted, QUOTA))
    }
}

/// A file as MEGA stores it: AES-CTR under its key, with its MAC.
struct Encrypted(FileKey);

impl Encoding for Encrypted {
    fn decoder(&self, offset: u64) -> Box<dyn Decoder> {
        Box::new(self.0.decryptor(offset))
    }

    fn verifier(&self) -> Box<dyn Verifier> {
        Box::new(self.0.mac_check())
    }
}

impl Decoder for crypto::Decryptor {
    fn decode(&mut self, data: &mut [u8]) {
        self.apply(data);
    }
}

impl Verifier for MacCheck {
    fn update(&mut self, data: &[u8]) {
        MacCheck::update(self, data);
    }

    fn verify(self: Box<Self>) -> bool {
        MacCheck::verify(*self)
    }
}
