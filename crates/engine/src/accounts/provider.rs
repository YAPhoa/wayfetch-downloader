//! The seams between the engine's account service and the code that knows
//! a provider's API (architecture brief sections 3.11, 6.4, 8.3, ADR-08,
//! ADR-15).
//!
//! * An [`AccountProvider`] speaks one provider's authorization protocol:
//!   it starts a device authorization, polls it, and checks stored
//!   credentials, refreshing them when the provider allows. It keeps no
//!   state: the engine owns pending authorizations, stored credentials,
//!   revisions, and single-flight coordination, and hands the provider only
//!   the secret each call needs.
//! * An [`ApiClient`] sends a provider's metadata requests (section 6.5's
//!   policies for metadata, not transfers): one bounded request, a whole
//!   bounded body, no redirects followed, no decompression. The host
//!   supplies the real one (`wayfetch-net`), so egress, TLS, and proxy
//!   policies stay in one place and tests run against fixtures.
//!
//! Errors carry no secret: providers build them from status codes and the
//! provider's own error codes, never from a request or a raw body.

use std::time::Duration;

use url::Url;
use wayfetch_domain::name::ProviderId;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::account::{AccountPlan, ServiceSupport};

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
    /// The network policy refused the destination, a redirect, or the
    /// response's encoding.
    Refused,
    /// The body was longer than allowed.
    TooLarge,
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
/// distinguishes (section 8.3). Messages name no secret.
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

/// One provider's account authorization.
pub trait AccountProvider: Send + Sync + 'static {
    /// The service, as named in the UI's catalogue.
    fn service(&self) -> &ProviderId;

    /// What the integration supports.
    fn support(&self) -> ServiceSupport;

    /// Starts a device authorization.
    fn begin(&self) -> BoxFuture<'_, Result<DeviceAuthorization, ProviderError>>;

    /// Polls a device authorization once.
    fn poll<'a>(&'a self, pending: &'a Secret)
    -> BoxFuture<'a, Result<PollOutcome, ProviderError>>;

    /// Checks stored credentials with the provider, refreshing them first
    /// if they have expired or the provider says they have.
    fn check<'a>(&'a self, credential: &'a Secret) -> BoxFuture<'a, Checked>;
}
