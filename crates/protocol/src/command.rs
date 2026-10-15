//! Commands: durable, idempotent mutations (architecture brief sections 4.6,
//! 7.2 and 11.1).
//!
//! Every command travels in a [`CommandEnvelope`] carrying a client-chosen
//! idempotency key and the target instance. The engine stores the key with
//! the command's [`CommandFingerprint`] and its result in the same
//! transaction as the state change, and reports success only after that
//! commit. A retried envelope with the same key and fingerprint replays the
//! stored result; the same key with a different fingerprint is
//! [`crate::ErrorCode::IdempotencyConflict`] (test T46).
//!
//! Commands that edit a job carry the revision the client last saw, and
//! conflict if the job has changed since.
//!
//! The principal is never part of the payload. The host adapter (HTTP or
//! desktop IPC) authenticates the caller and the engine checks
//! [`Command::required_permission`].

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;
use wayfetch_domain::id::{AccountId, BatchId, JobId, PackageId, RootId, SourceLinkId};
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::version::Revision;

use crate::account::{AccountAlias, AccountRef, Authorization};
use crate::error::{ErrorCode, ProtocolError};
use crate::instance::InstanceId;

/// Maximum inputs in one [`Command::Submit`].
pub const MAX_SUBMIT_ITEMS: usize = 1000;
/// Maximum length of one submitted URL in bytes.
pub const MAX_URL_BYTES: usize = 8192;
/// Maximum links waiting in the collected list, across every batch. A
/// collect command that would exceed it is refused whole. Added in
/// protocol 1.1.
pub const MAX_COLLECTED_LINKS: usize = 10_000;

/// A client-chosen key identifying one logical command across retries. A
/// UUID, so clients can generate one without coordination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct IdempotencyKey(Uuid);

impl IdempotencyKey {
    /// A fresh random key.
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps a stored or client-supplied key.
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The key.
    pub const fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

/// What a principal must be allowed to do to run a command or query.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Permission {
    /// Read queries and subscribe to events.
    Read,
    /// Submit new inputs.
    Submit,
    /// Pause, resume, cancel, retry, rename, remove, and otherwise control
    /// existing jobs.
    Control,
    /// Read the links jobs were submitted with, which can carry signed
    /// tokens and so are not part of what [`Permission::Read`] shows.
    /// Added in protocol 1.0.
    Sources,
    /// See and manage provider accounts: list them, sign them in, check,
    /// enable, disable, and remove them. No other permission implies it,
    /// since accounts hold the user's provider credentials. Added in
    /// protocol 1.3; clients treat permissions they do not know as absent.
    Accounts,
}

/// One URL to ingest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct InputUrl(Url);

impl InputUrl {
    /// Parses and validates submitted text. The raw text is bounded by
    /// [`MAX_URL_BYTES`] before parsing, because parsing strips characters
    /// such as embedded newlines and tabs; the parsed URL is then checked by
    /// [`InputUrl::new`].
    pub fn parse(raw: &str) -> Result<Self, ProtocolError> {
        if raw.len() > MAX_URL_BYTES {
            return Err(url_too_long());
        }
        let url = Url::parse(raw)
            .map_err(|_| ProtocolError::new(ErrorCode::InvalidRequest, "not a valid URL"))?;
        Self::new(url)
    }

    /// Validates a parsed URL: `http` or `https`, at most [`MAX_URL_BYTES`]
    /// after normalization, with a host and no embedded credentials.
    /// Credentials belong to accounts, never to queue entries.
    pub fn new(url: Url) -> Result<Self, ProtocolError> {
        let invalid = |m: &str| ProtocolError::new(ErrorCode::InvalidRequest, m);
        if !matches!(url.scheme(), "http" | "https") {
            return Err(invalid("only http and https URLs are supported"));
        }
        if url.as_str().len() > MAX_URL_BYTES {
            return Err(url_too_long());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("URLs must not contain credentials"));
        }
        if url.host().is_none() {
            return Err(invalid("URL has no host"));
        }
        Ok(Self(url))
    }

    /// The validated URL.
    pub fn as_url(&self) -> &Url {
        &self.0
    }
}

fn url_too_long() -> ProtocolError {
    ProtocolError::new(ErrorCode::LimitExceeded, "URL too long")
}

impl std::str::FromStr for InputUrl {
    type Err = ProtocolError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

impl TryFrom<String> for InputUrl {
    type Error = ProtocolError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<InputUrl> for String {
    fn from(value: InputUrl) -> Self {
        value.0.into()
    }
}

/// A bounded, non-empty list of inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<InputUrl>", into = "Vec<InputUrl>")]
pub struct SubmitItems(Vec<InputUrl>);

impl SubmitItems {
    /// The inputs, in submission order.
    pub fn as_slice(&self) -> &[InputUrl] {
        &self.0
    }
}

impl TryFrom<Vec<InputUrl>> for SubmitItems {
    type Error = ProtocolError;

    fn try_from(items: Vec<InputUrl>) -> Result<Self, Self::Error> {
        bounded(&items, "no inputs")?;
        Ok(Self(items))
    }
}

impl From<SubmitItems> for Vec<InputUrl> {
    fn from(value: SubmitItems) -> Self {
        value.0
    }
}

/// Text to collect: a bounded, non-empty list of entries, each at most
/// [`MAX_URL_BYTES`]. Entries are kept as entered, not validated as URLs
/// here: the engine records an entry that is not a valid link as invalid,
/// with its problem, beside the valid ones. Added in protocol 1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct CollectItems(Vec<String>);

impl CollectItems {
    /// The entries, in submission order.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

impl TryFrom<Vec<String>> for CollectItems {
    type Error = ProtocolError;

    fn try_from(items: Vec<String>) -> Result<Self, Self::Error> {
        bounded(&items, "no links")?;
        if let Some(i) = items.iter().position(|item| item.len() > MAX_URL_BYTES) {
            return Err(ProtocolError::new(
                ErrorCode::LimitExceeded,
                &format!("item {} rejected: URL too long", i + 1),
            ));
        }
        Ok(Self(items))
    }
}

impl From<CollectItems> for Vec<String> {
    fn from(value: CollectItems) -> Self {
        value.0
    }
}

/// A collected link, at the revision the client last saw. Added in
/// protocol 1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LinkRef {
    /// The link.
    pub link: SourceLinkId,
    /// Its revision as the client saw it.
    pub revision: Revision,
}

/// A bounded, non-empty list of collected links, each named once. Added
/// in protocol 1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<LinkRef>", into = "Vec<LinkRef>")]
pub struct LinkRefs(Vec<LinkRef>);

impl LinkRefs {
    /// The links, in the order given.
    pub fn as_slice(&self) -> &[LinkRef] {
        &self.0
    }
}

impl TryFrom<Vec<LinkRef>> for LinkRefs {
    type Error = ProtocolError;

    fn try_from(links: Vec<LinkRef>) -> Result<Self, Self::Error> {
        bounded(&links, "no links")?;
        let mut seen = std::collections::HashSet::with_capacity(links.len());
        if !links.iter().all(|l| seen.insert(l.link)) {
            return Err(ProtocolError::new(
                ErrorCode::InvalidRequest,
                "a link is named more than once",
            ));
        }
        Ok(Self(links))
    }
}

impl From<LinkRefs> for Vec<LinkRef> {
    fn from(value: LinkRefs) -> Self {
        value.0
    }
}

/// Checks a command's list holds 1 to [`MAX_SUBMIT_ITEMS`] entries.
fn bounded<T>(items: &[T], empty: &str) -> Result<(), ProtocolError> {
    match items.len() {
        0 => Err(ProtocolError::new(ErrorCode::InvalidRequest, empty)),
        n if n > MAX_SUBMIT_ITEMS => Err(ProtocolError::new(
            ErrorCode::LimitExceeded,
            "too many inputs",
        )),
        _ => Ok(()),
    }
}

/// A mutation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Ingest inputs as a new batch. Ingestion is a workflow (crawl, check,
    /// stage); the result is the durable batch ID, not finished jobs.
    Submit {
        /// The inputs.
        items: SubmitItems,
        /// Package to add resulting jobs to; the engine creates one if
        /// absent.
        package: Option<PackageId>,
    },
    /// Pause a job at a safe checkpoint, keeping partial data.
    Pause {
        /// The job.
        job: JobId,
        /// The job revision the client last saw.
        expected_revision: Revision,
    },
    /// Withdraw a pause.
    Resume {
        /// The job.
        job: JobId,
        /// The job revision the client last saw.
        expected_revision: Revision,
    },
    /// End the job's current operation and stop automatic retries, keeping
    /// data. In-flight results of the old generation are discarded.
    Cancel {
        /// The job.
        job: JobId,
        /// The job revision the client last saw.
        expected_revision: Revision,
    },
    /// Start a failed or cancelled job again as a new attempt of the same
    /// job, keeping its source, destination, and collected link. The
    /// attempt continues the job's verified saved data when it safely can,
    /// and otherwise starts from zero and says why. A failure whose retry
    /// deadline has not passed is refused. Added in protocol 1.2.
    Retry {
        /// The job.
        job: JobId,
        /// The job revision the client last saw.
        expected_revision: Revision,
    },
    /// Choose where jobs submitted from now on are saved. Jobs already
    /// submitted keep their destination. Added in protocol 0.3.
    UpdateSettings {
        /// The settings revision the client last saw.
        expected_revision: Revision,
        /// One of the roots the settings list, which must be available.
        default_root: RootId,
    },
    /// Give a completed job's published file another name in the same
    /// folder. Never replaces an existing file. Added in protocol 1.0.
    Rename {
        /// The job.
        job: JobId,
        /// The job revision the client last saw.
        expected_revision: Revision,
        /// The new file name: one path component, checked against the
        /// engine's filename policy.
        name: String,
    },
    /// Remove a finished, failed, or cancelled job from lists and counts.
    /// Its files, partial data, and records stay. Added in protocol 1.0.
    Remove {
        /// The job.
        job: JobId,
        /// The job revision the client last saw.
        expected_revision: Revision,
    },
    /// Stop offering a folder the user added as a destination. Its files
    /// stay; a folder the operator configured, the current default, or one
    /// that unfinished jobs save to is refused. Added in protocol 1.1.
    RemoveRoot {
        /// The settings revision the client last saw.
        expected_revision: Revision,
        /// The folder.
        root: RootId,
    },
    /// Collect links for review, as a new batch: nothing is downloaded or
    /// fetched until they are started. An entry that is not a valid link is
    /// kept as invalid, with its problem. Added in protocol 1.1.
    CollectLinks {
        /// The entries.
        items: CollectItems,
        /// Where the links will be saved: a permitted root that is
        /// available now. The default destination if absent.
        destination: Option<RootId>,
    },
    /// Turn collected links into downloads, each once; they stay listed
    /// with their download until removed. A link that changed since the
    /// client saw it, is invalid, or whose destination cannot be used is
    /// refused and stays collected. Added in protocol 1.1.
    StartLinks {
        /// The links.
        links: LinkRefs,
    },
    /// Remove collected links from the list. Only their collected records
    /// go; downloads already started are not affected. Added in protocol
    /// 1.1.
    RemoveLinks {
        /// The links.
        links: LinkRefs,
    },
    /// Choose where collected links will be saved. The default destination
    /// and other links keep theirs, and a link already started is refused.
    /// Added in protocol 1.1.
    MoveLinks {
        /// The links.
        links: LinkRefs,
        /// A permitted root that is available now.
        destination: RootId,
    },
    /// Sign in to a provider: a new account of `service`, or `account`
    /// again to replace its credentials. Starts an authorization that the
    /// person approves on the provider's site; the account is added, or
    /// its credentials replaced, only once they have. Carries no
    /// credentials. A retry with the same key answers with the same
    /// authorization, or with the account it added. Added in protocol 1.3.
    AuthorizeAccount {
        /// The service.
        service: ProviderId,
        /// The account to sign in again, at the revision the client saw;
        /// absent to add one.
        #[serde(default)]
        account: Option<AccountRef>,
        /// A name for a new account.
        #[serde(default)]
        alias: Option<AccountAlias>,
    },
    /// Enable or disable an account. A disabled account is kept, with its
    /// credentials, but its provider is not contacted. Added in protocol
    /// 1.3.
    SetAccountEnabled {
        /// The account.
        account: AccountId,
        /// The account revision the client last saw.
        expected_revision: Revision,
        /// Whether it may be used.
        enabled: bool,
    },
    /// Remove an account and its stored credentials. Work for it that has
    /// not finished is discarded. Added in protocol 1.3.
    RemoveAccount {
        /// The account.
        account: AccountId,
        /// The account revision the client last saw.
        expected_revision: Revision,
    },
}

impl Permission {
    /// Every permission: what the local owner holds.
    pub const ALL: &'static [Self] = <Self as strum::VariantArray>::VARIANTS;
}

impl Command {
    /// The permission the caller needs.
    pub const fn required_permission(&self) -> Permission {
        match self {
            Self::Submit { .. } | Self::CollectLinks { .. } | Self::StartLinks { .. } => {
                Permission::Submit
            }
            // The list is shared: changing or dropping entries someone
            // else collected is control, like changing their downloads.
            Self::RemoveLinks { .. }
            | Self::MoveLinks { .. }
            | Self::Pause { .. }
            | Self::Resume { .. }
            | Self::Cancel { .. }
            | Self::Retry { .. }
            | Self::UpdateSettings { .. }
            | Self::Rename { .. }
            | Self::Remove { .. }
            | Self::RemoveRoot { .. } => Permission::Control,
            Self::AuthorizeAccount { .. }
            | Self::SetAccountEnabled { .. }
            | Self::RemoveAccount { .. } => Permission::Accounts,
        }
    }

    /// The command's fingerprint: SHA-256 of its canonical JSON encoding.
    /// Field order is fixed by the type definitions and the payload holds no
    /// maps, so equal commands always have equal fingerprints.
    pub fn fingerprint(&self) -> CommandFingerprint {
        let bytes = serde_json::to_vec(self).expect("commands always serialize");
        CommandFingerprint(Sha256::digest(bytes).into())
    }
}

/// A command with its idempotency key and target instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    /// Identifies this logical command across retries.
    pub idempotency_key: IdempotencyKey,
    /// The instance the client believes it is talking to.
    pub target: InstanceId,
    /// The command.
    pub command: Command,
}

/// Stable digest of a command, stored with its idempotency key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandFingerprint([u8; 32]);

impl CommandFingerprint {
    /// Wraps a stored digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The digest, for storage.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The durable result of an accepted command; replayed verbatim for a
/// retried envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum CommandResult {
    /// A batch was accepted.
    Submitted {
        /// The durable batch.
        batch: BatchId,
        /// Number of inputs accepted.
        accepted: u32,
    },
    /// A job request was applied.
    JobUpdated {
        /// The job.
        job: JobId,
        /// The job's revision after the command; unchanged if the job was
        /// already in the requested state.
        revision: Revision,
        /// Whether the command changed anything.
        changed: bool,
    },
    /// The settings were updated, including the destinations offered.
    /// Added in protocol 0.3.
    SettingsUpdated {
        /// The settings revision after the command; unchanged if they
        /// already said so.
        revision: Revision,
        /// Whether the command changed anything.
        changed: bool,
    },
    /// Links were collected. Added in protocol 1.1.
    LinksCollected {
        /// The batch they were collected as.
        batch: BatchId,
        /// Entries collected as valid links.
        collected: u32,
        /// Entries kept as invalid.
        invalid: u32,
    },
    /// A command on collected links was applied to some of them and
    /// refused for the rest, which stay as they were. Added in protocol
    /// 1.1.
    LinksApplied {
        /// The links it applied to.
        applied: Vec<AppliedLink>,
        /// The links it was refused for.
        refused: Vec<RefusedLink>,
    },
    /// An authorization started, or is still waiting, for
    /// [`Command::AuthorizeAccount`]: show its code. Added in protocol 1.3.
    AuthorizationStarted(Authorization),
    /// An authorization completed: the account added or signed in again.
    /// What a retry of [`Command::AuthorizeAccount`] answers once it has.
    /// Added in protocol 1.3.
    AccountAuthorized {
        /// The account.
        account: AccountId,
        /// Its revision afterwards.
        revision: Revision,
    },
    /// An account was enabled, disabled, or removed. Added in protocol 1.3.
    AccountUpdated {
        /// The account.
        account: AccountId,
        /// Its revision after the command; for a removal, the revision it
        /// had.
        revision: Revision,
        /// Whether the command changed anything.
        changed: bool,
    },
}

/// A collected link a command applied to. Added in protocol 1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AppliedLink {
    /// The link.
    pub link: SourceLinkId,
    /// The download it started as, for a start; also for a link started
    /// before, which is never started twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub job: Option<JobId>,
    /// Its revision after a destination change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub revision: Option<Revision>,
}

/// A collected link a command was refused for. Added in protocol 1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RefusedLink {
    /// The link.
    pub link: SourceLinkId,
    /// Why, as an error code: `not_found` for a link that was removed,
    /// `revision_conflict` for one that changed, `not_applicable` for an
    /// invalid link, one already started, or a destination that cannot be
    /// used.
    pub code: ErrorCode,
    /// What to do about it.
    pub message: DiagnosticText,
}

/// What a stored idempotency record says about a newly received envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay<'a> {
    /// Same key and fingerprint: return the stored result, do nothing else.
    Stored(&'a CommandResult),
    /// Same key, different command: reject.
    Conflict,
}

/// A stored command outcome, keyed by idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyRecord {
    /// The fingerprint of the command first accepted under the key.
    pub fingerprint: CommandFingerprint,
    /// Its result.
    pub result: CommandResult,
}

impl IdempotencyRecord {
    /// Decides how to answer `command` received under this record's key.
    pub fn replay(&self, command: &Command) -> Replay<'_> {
        if command.fingerprint() == self.fingerprint {
            Replay::Stored(&self.result)
        } else {
            Replay::Conflict
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn url(s: &str) -> Result<InputUrl, ProtocolError> {
        InputUrl::parse(s)
    }

    fn pause(job: u64, revision: u64) -> Command {
        Command::Pause {
            job: JobId::from_u64(job).unwrap(),
            expected_revision: Revision::from_u64(revision).unwrap(),
        }
    }

    #[test]
    fn input_urls_are_validated() {
        assert!(url("https://files.example/a.bin").is_ok());
        assert!(url("http://192.0.2.1/a").is_ok());
        for bad in [
            "ftp://files.example/a",
            "https://user:pw@files.example/a",
            "file:///etc/passwd",
        ] {
            assert!(url(bad).is_err(), "{bad} accepted");
        }
        let long = format!("https://files.example/{}", "a".repeat(MAX_URL_BYTES));
        assert_eq!(url(&long).unwrap_err().code, ErrorCode::LimitExceeded);
    }

    #[test]
    fn raw_url_length_is_checked_before_parsing() {
        // Parsing strips embedded newlines and tabs, so the normalized URL
        // alone would fit the limit.
        let padded = format!("https://files.example/a{}", "\n".repeat(MAX_URL_BYTES + 1));
        assert!(Url::parse(&padded).unwrap().as_str().len() <= MAX_URL_BYTES);
        assert_eq!(url(&padded).unwrap_err().code, ErrorCode::LimitExceeded);
        let json = json!({"type": "submit", "items": [padded], "package": null});
        let err = serde_json::from_value::<Command>(json).unwrap_err();
        assert!(err.to_string().contains("URL too long"), "{err}");
        // A URL whose percent-encoded form exceeds the limit is still refused.
        let encoded = format!("https://files.example/{}z", " ".repeat(MAX_URL_BYTES / 3));
        assert!(encoded.len() <= MAX_URL_BYTES);
        assert_eq!(url(&encoded).unwrap_err().code, ErrorCode::LimitExceeded);
    }

    #[test]
    fn submit_is_bounded_and_non_empty() {
        let item = url("https://files.example/a").unwrap();
        assert!(SubmitItems::try_from(vec![]).is_err());
        assert!(SubmitItems::try_from(vec![item.clone(); MAX_SUBMIT_ITEMS]).is_ok());
        let err = SubmitItems::try_from(vec![item; MAX_SUBMIT_ITEMS + 1]).unwrap_err();
        assert_eq!(err.code, ErrorCode::LimitExceeded);
    }

    #[test]
    fn wire_form_is_tagged_and_strict() {
        let json = json!({"type": "pause", "job": "job-3", "expected_revision": 2});
        assert_eq!(
            serde_json::from_value::<Command>(json).unwrap(),
            pause(3, 2)
        );
        // Unknown fields are rejected, so a client cannot smuggle privileges.
        let extra =
            json!({"type": "pause", "job": "job-3", "expected_revision": 2, "is_admin": true});
        assert!(serde_json::from_value::<Command>(extra).is_err());
        let bad_url = json!({"type": "submit", "items": ["ftp://x.example/"], "package": null});
        assert!(serde_json::from_value::<Command>(bad_url).is_err());
        let empty = json!({"type": "submit", "items": [], "package": null});
        assert!(serde_json::from_value::<Command>(empty).is_err());
    }

    #[test]
    fn idempotent_replay_and_conflict() {
        let record = IdempotencyRecord {
            fingerprint: pause(3, 2).fingerprint(),
            result: CommandResult::JobUpdated {
                job: JobId::from_u64(3).unwrap(),
                revision: Revision::from_u64(3).unwrap(),
                changed: true,
            },
        };
        assert_eq!(record.replay(&pause(3, 2)), Replay::Stored(&record.result));
        assert_eq!(record.replay(&pause(3, 1)), Replay::Conflict);
        assert_eq!(record.replay(&pause(4, 2)), Replay::Conflict);
    }

    #[test]
    fn fingerprint_is_stable() {
        // Pinned so an accidental wire-format change is caught: stored
        // fingerprints must keep matching after upgrades.
        let hex: String = pause(3, 2)
            .fingerprint()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let expected = {
            let canonical = br#"{"type":"pause","job":"job-3","expected_revision":2}"#;
            let digest: [u8; 32] = Sha256::digest(canonical).into();
            digest
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(hex, expected);
    }

    #[test]
    fn retry_fingerprint_is_its_canonical_form() {
        let retry = Command::Retry {
            job: JobId::from_u64(3).unwrap(),
            expected_revision: Revision::from_u64(2).unwrap(),
        };
        let canonical = br#"{"type":"retry","job":"job-3","expected_revision":2}"#;
        let digest: [u8; 32] = Sha256::digest(canonical).into();
        assert_eq!(retry.fingerprint().as_bytes(), &digest);
        // A retry never replays another command's record, or the reverse.
        assert_ne!(retry.fingerprint(), pause(3, 2).fingerprint());
    }

    #[test]
    fn permissions() {
        assert_eq!(pause(1, 1).required_permission(), Permission::Control);
        let submit = Command::Submit {
            items: SubmitItems::try_from(vec![url("https://a.example/").unwrap()]).unwrap(),
            package: None,
        };
        assert_eq!(submit.required_permission(), Permission::Submit);
        let remove = Command::Remove {
            job: JobId::from_u64(1).unwrap(),
            expected_revision: Revision::INITIAL,
        };
        assert_eq!(remove.required_permission(), Permission::Control);
        // Starting a job again is control, not submission.
        let retry = Command::Retry {
            job: JobId::from_u64(1).unwrap(),
            expected_revision: Revision::INITIAL,
        };
        assert_eq!(retry.required_permission(), Permission::Control);
        assert_eq!(
            "sources".parse::<Permission>().unwrap(),
            Permission::Sources
        );
        let disable = Command::SetAccountEnabled {
            account: AccountId::from_u64(1).unwrap(),
            expected_revision: Revision::INITIAL,
            enabled: false,
        };
        assert_eq!(disable.required_permission(), Permission::Accounts);
        assert_eq!(
            "accounts".parse::<Permission>().unwrap(),
            Permission::Accounts
        );
    }

    #[test]
    fn signing_in_carries_no_credentials() {
        // A client cannot send a secret, so none reaches the fingerprint
        // or the stored command.
        let json = json!({
            "type": "authorize_account",
            "service": "real-debrid",
            "account": null,
            "alias": "Home",
        });
        let command = serde_json::from_value::<Command>(json).unwrap();
        assert_eq!(command.required_permission(), Permission::Accounts);
        let with_secret = json!({
            "type": "authorize_account",
            "service": "real-debrid",
            "token": "secret",
        });
        assert!(serde_json::from_value::<Command>(with_secret).is_err());
    }
}
