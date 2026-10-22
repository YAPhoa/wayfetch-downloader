//! The seams between the engine's account service and a provider's API
//! (ADR-08, ADR-15).
//!
//! An [`AccountProvider`] speaks one provider's sign-in protocol and keeps
//! no state; the engine owns accounts, credentials, and coordination. An
//! [`ApiClient`] sends one bounded metadata request under the host's network
//! policy. Errors never carry a secret.

use std::time::Duration;

use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::name::{HostName, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::account::{AccountPlan, ServiceSupport};
use wayfetch_protocol::query::CheckReason;

use super::Secret;
use crate::transfer::writer::BoxFuture;

/// The method of a metadata request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMethod {
    /// `GET`, sent once; never repeated by the client.
    Get,
    /// `POST` with a form body, sent once.
    Post,
}

/// One metadata request to a provider's API.
pub struct ApiRequest {
    /// The method.
    pub method: ApiMethod,
    /// The URL. Tokens go in `bearer` or `form`; a secret goes in the query
    /// only where the provider's protocol requires it (Real-Debrid's device
    /// code), so an API client never reports the URL in an error.
    pub url: Url,
    /// A bearer token for the `Authorization` header.
    pub bearer: Option<Secret>,
    /// An `application/x-www-form-urlencoded` body, already encoded.
    pub form: Option<Secret>,
    /// Deadline for the whole exchange, body included.
    pub deadline: Duration,
    /// Largest body accepted; a longer one is refused, not truncated.
    pub max_body: usize,
}

impl std::fmt::Debug for ApiRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiRequest")
            .field("method", &self.method)
            .field("origin", &self.url.origin().ascii_serialization())
            .field("path", &self.url.path())
            .finish_non_exhaustive()
    }
}

/// A provider's answer: any status, with its whole body.
#[derive(Debug)]
pub struct ApiResponse {
    /// The HTTP status.
    pub status: u16,
    /// The body, which may carry tokens.
    pub body: Secret,
}

/// Why a metadata request produced no response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub enum ApiErrorKind {
    /// The provider could not be reached, or the connection failed.
    Unreachable,
    /// The deadline passed.
    Timeout,
    /// The network policy refused the destination, or the request could
    /// not be built: it was never sent.
    Refused,
    /// The provider answered, but its answer was refused unread: a
    /// redirect, or an encoded body. The provider may have acted on the
    /// request.
    Unreadable,
    /// The body was longer than allowed. The provider may have acted on
    /// the request.
    TooLarge,
    /// The connection failed after the request may have reached the
    /// provider: unlike [`Self::Unreachable`], the provider may have acted
    /// on it.
    Interrupted,
}

/// A metadata request failed. Its message names no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct ApiError {
    /// What went wrong.
    pub kind: ApiErrorKind,
    /// A short description for diagnostics.
    pub message: String,
}

/// Sends a provider's metadata requests.
pub trait ApiClient: Send + Sync + 'static {
    /// Sends `request` once and reads its whole body within its deadline
    /// and size limit.
    fn send<'a>(&'a self, request: &'a ApiRequest) -> BoxFuture<'a, Result<ApiResponse, ApiError>>;
}

/// What the provider says about an account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    /// The provider's stable ID for the account, so one account is not
    /// added twice.
    pub user_id: Option<String>,
    /// Its name at the provider.
    pub username: Option<String>,
    /// Its plan.
    pub plan: Option<AccountPlan>,
    /// When its paid plan ends; absent when the provider did not say.
    pub expires_at: Option<UtcTimestamp>,
}

/// A device authorization the provider started.
#[derive(Debug)]
pub struct DeviceAuthorization {
    /// The code the person enters on the provider's site.
    pub user_code: String,
    /// Where they enter it.
    pub verification_url: Url,
    /// How long the code lasts.
    pub expires_in: Duration,
    /// How long to wait between polls.
    pub interval: Duration,
    /// What polling needs, such as the device code. Never shown.
    pub pending: Secret,
}

/// The result of one poll of a device authorization.
#[derive(Debug)]
pub enum PollOutcome {
    /// Not approved yet.
    Pending,
    /// Not approved yet, and the provider asks for slower polling.
    SlowDown,
    /// Approved: the credentials to store, and what the provider said
    /// about the account. Without the provider's ID for the account
    /// (`profile.user_id`), the engine refuses the sign-in: it cannot tell
    /// a new account from one already added, or confirm a replacement.
    Authorized {
        /// The account's credentials, in the provider's own format.
        credential: Secret,
        /// The account.
        profile: Profile,
    },
    /// The person or the provider refused.
    Denied(String),
    /// The code expired.
    Expired,
}

/// The result of checking stored credentials.
#[derive(Debug)]
pub struct Checked {
    /// The account, or why it could not be confirmed.
    pub result: Result<Profile, ProviderError>,
    /// Credentials to store instead, when the check refreshed them, whether
    /// or not it then confirmed the account: a provider that rotates
    /// refresh tokens may no longer accept the old ones.
    pub refreshed: Option<Secret>,
}

impl Checked {
    /// A check that failed before any credentials changed.
    pub fn failed(error: ProviderError) -> Self {
        Self {
            result: Err(error),
            refreshed: None,
        }
    }
}

/// Why a provider call failed, in the categories the account's status
/// distinguishes. Messages name no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider refused the credentials: they are invalid, revoked, or
    /// expired without a way to refresh them.
    #[error("{0}")]
    InvalidCredentials(String),
    /// The provider could not be reached, or failed temporarily. Trying
    /// later may work.
    #[error("{0}")]
    Unavailable(String),
    /// The provider answered with something this integration does not
    /// understand, such as a malformed or oversized body.
    #[error("{0}")]
    Malformed(String),
}

/// A download link a provider generated for an account (Real-Debrid's
/// "unrestricted" link). The URL is a bearer capability: it never prints,
/// and the engine keeps it sealed.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedLink {
    /// Where the file is downloaded from. Checked by the provider: HTTPS,
    /// without credentials.
    pub url: Url,
    /// The name the provider suggests: an untrusted hint, checked like a
    /// server's suggestion before use.
    pub filename: Option<String>,
    /// The size the provider reports; `None` when it does not know.
    pub size: Option<u64>,
    /// Checksums the provider gives for the file's bytes, checked before
    /// it is published.
    pub checksums: Vec<crate::checksum::Digest>,
}

impl std::fmt::Debug for GeneratedLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedLink")
            .field("url", &"[redacted]")
            .field("filename", &self.filename)
            .field("size", &self.size)
            .field("checksums", &self.checksums)
            .finish()
    }
}

/// What asking a provider for a download link found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrestricted {
    /// The provider generated a link.
    Generated(GeneratedLink),
    /// The provider certainly did not generate one: it refused, or the
    /// request never reached it. `code` is the failure to record; the
    /// message says what the person can do and names no secret.
    Refused {
        /// The failure.
        code: FailureCode,
        /// What happened.
        message: String,
        /// When trying again may work, if the provider said.
        retry_after: Option<Duration>,
    },
    /// The request may have reached the provider, but its answer was lost
    /// or could not be read: the provider may have generated a link. The
    /// engine reconciles rather than asking again.
    Ambiguous(String),
}

impl Unrestricted {
    /// A refusal with no hint of when to try again.
    pub fn refused(code: FailureCode, message: impl Into<String>) -> Self {
        Self::Refused {
            code,
            message: message.into(),
            retry_after: None,
        }
    }
}

/// What looking for a link the provider may already have generated found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciled {
    /// The provider lists a link generated for the original link.
    Found(GeneratedLink),
    /// The provider's record answered and lists none.
    NotFound,
    /// The provider's record was read, but cannot show whether the request
    /// generated a link: several entries, or one in another form, may be
    /// its outcome, or it reaches back too little. Reading it again will
    /// not tell.
    Inconclusive(String),
    /// The provider's record could not be read; nothing is known.
    Unknown(String),
}

/// A provider call made with an account's credentials, and the
/// credentials to store instead when it refreshed them, whether or not
/// the call then succeeded.
#[derive(Debug)]
pub struct WithCredentials<T> {
    /// What the call found.
    pub result: T,
    /// Refreshed credentials.
    pub refreshed: Option<Secret>,
}

impl<T> WithCredentials<T> {
    /// A result reached without changing the credentials.
    pub fn unchanged(result: T) -> Self {
        Self {
            result,
            refreshed: None,
        }
    }
}

/// What a provider says about a link it would download, without
/// generating anything or using an account: a link check of a hoster's
/// link, which the hoster itself refuses to answer anonymously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// The provider can download the file.
    File {
        /// The name the provider reported: an untrusted hint.
        name: Option<String>,
        /// Its size, when the provider knows it.
        size: Option<u64>,
    },
    /// The provider says the file is gone.
    Offline(String),
    /// The provider could not tell; `reason` in a link check's terms.
    Unknown(CheckReason, String),
}

/// A bounded, validated provider host-domain catalogue. Host identity is
/// parsed separately from URLs; matches include subdomains at a dot boundary.
#[derive(Debug, Clone)]
pub struct SupportedHosts(Vec<HostName>);

impl SupportedHosts {
    /// Validate at most 1024 DNS domains, each at most 253 bytes. An invalid
    /// catalogue is unknown support, never an empty/nonmatching catalogue.
    pub fn new(domains: Vec<String>) -> Result<Self, ProviderError> {
        if domains.len() > 1024 {
            return Err(ProviderError::Malformed(
                "too many supported host domains".into(),
            ));
        }
        let hosts = domains
            .into_iter()
            .map(|domain| {
                if domain.len() > 253
                    || !domain
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
                {
                    return Err(ProviderError::Malformed(
                        "invalid supported host domain".into(),
                    ));
                }
                let host = HostName::new(domain).map_err(|_| {
                    ProviderError::Malformed("invalid supported host domain".into())
                })?;
                if !matches!(url::Host::parse(host.as_str()), Ok(url::Host::Domain(_)))
                    || host
                        .as_str()
                        .split('.')
                        .any(|label| label.starts_with('-') || label.ends_with('-'))
                {
                    return Err(ProviderError::Malformed(
                        "supported host must be a DNS domain".into(),
                    ));
                }
                Ok(host)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(hosts))
    }

    /// Whether a parsed URL's hostname is this domain or a subdomain.
    pub fn supports(&self, url: &Url) -> bool {
        let Some(raw) = url.host_str() else {
            return false;
        };
        let Ok(host) = HostName::new(raw) else {
            return false;
        };
        self.0.iter().any(|domain| {
            host == *domain
                || host
                    .as_str()
                    .strip_suffix(domain.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    }
}

/// One provider's account authorization.
pub trait AccountProvider: Send + Sync + 'static {
    /// The service, as named in the UI's catalogue.
    fn service(&self) -> &ProviderId;

    /// What the integration supports.
    fn support(&self) -> ServiceSupport;

    /// Whether this provider recognizes an account-only native file link.
    /// Pure recognition only: no network or credential access. Automatic
    /// routing must use this provider's account, never a different provider
    /// or an implicit Direct fallback, even without host metadata.
    fn requires_account(&self, _link: &Url) -> bool {
        false
    }

    /// Read the provider's supported domains without generating a download
    /// or consuming account quota. Failure means unknown support.
    fn supported_hosts(&self) -> BoxFuture<'_, Result<SupportedHosts, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::Unavailable(
                "supported hosts are not available for this service".into(),
            ))
        })
    }

    /// Asks the provider whether it can download `link`, without
    /// credentials, generating a link, or spending account quota (ADR-08:
    /// a check is advisory). Unknown by default.
    fn availability<'a>(&'a self, _link: &'a Url) -> BoxFuture<'a, Availability> {
        Box::pin(async {
            Availability::Unknown(
                CheckReason::Unreadable,
                "this service cannot check links".into(),
            )
        })
    }

    /// Starts a device authorization.
    fn begin(&self) -> BoxFuture<'_, Result<DeviceAuthorization, ProviderError>>;

    /// Polls a device authorization once.
    fn poll<'a>(&'a self, pending: &'a Secret)
    -> BoxFuture<'a, Result<PollOutcome, ProviderError>>;

    /// Checks stored credentials with the provider, refreshing them first
    /// if they have expired or the provider says they have.
    fn check<'a>(&'a self, credential: &'a Secret) -> BoxFuture<'a, Checked>;

    /// Asks the provider, once, for a download link for the original
    /// `link` through the account. Not idempotent: the provider may count
    /// or record each request, so the engine records its intent first and
    /// never repeats a request whose answer was lost; it reconciles with
    /// [`Self::find_generated`] instead. Only for a provider whose
    /// [`ServiceSupport::downloads`] is true.
    fn unrestrict<'a>(
        &'a self,
        _credential: &'a Secret,
        _link: &'a Url,
    ) -> BoxFuture<'a, WithCredentials<Unrestricted>> {
        Box::pin(async {
            WithCredentials::unchanged(Unrestricted::refused(
                FailureCode::UnsupportedCapability,
                "this service cannot download yet",
            ))
        })
    }

    /// Looks, without generating anything, for a link the provider
    /// generated for `link` through the account at or after `since`: the
    /// evidence that an earlier request whose answer was lost succeeded.
    fn find_generated<'a>(
        &'a self,
        _credential: &'a Secret,
        _link: &'a Url,
        _since: UtcTimestamp,
    ) -> BoxFuture<'a, WithCredentials<Reconciled>> {
        Box::pin(async {
            WithCredentials::unchanged(Reconciled::Unknown(
                "this service keeps no record of generated links".into(),
            ))
        })
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn domains_match_only_parsed_hostname_boundaries() {
        let hosts = SupportedHosts::new(vec!["HOSTER.Example.".into()]).unwrap();
        for url in [
            "https://hoster.example/a",
            "https://HOSTER.EXAMPLE/a",
            "https://cdn.hoster.example/a",
            "https://hoster.example./a",
        ] {
            assert!(hosts.supports(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "https://nothoster.example/a",
            "https://hoster.example.evil.test/a",
            "https://evil.test/?next=https://hoster.example/",
            "https://hoster.example@evil.test/a",
            "https://127.0.0.1/a",
        ] {
            assert!(!hosts.supports(&Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn invalid_or_oversized_catalogues_are_unknown_not_no_match() {
        for domain in [
            "",
            "*.example",
            "https://example.com",
            "example.com/a",
            "example.com:443",
            "example..com",
            "-bad.example",
            "bad-.example",
            "127.0.0.1",
            "::1",
            "example.com@evil.test",
            " example.com",
        ] {
            assert!(
                SupportedHosts::new(vec![domain.into()]).is_err(),
                "{domain}"
            );
        }
        assert!(SupportedHosts::new(vec!["example.com".into(); 1025]).is_err());
        assert!(SupportedHosts::new(vec![format!("{}.example", "a".repeat(254))]).is_err());
        assert!(
            !SupportedHosts::new(Vec::new())
                .unwrap()
                .supports(&Url::parse("https://hoster.example/").unwrap())
        );
    }
}
