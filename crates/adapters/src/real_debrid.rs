//! Real-Debrid account sign-in and status (architecture brief sections
//! 3.11 and 8.3).
//!
//! Signing in uses Real-Debrid's documented OAuth 2.0 device flow for
//! open-source applications (<https://api.real-debrid.com/>, section
//! "Authentication"): the device code is requested with the client ID
//! Real-Debrid publishes for open-source apps, [`OPEN_SOURCE_CLIENT_ID`],
//! and `new_credentials=yes`; the person enters the code on Real-Debrid's
//! site; Real-Debrid then issues a client ID and secret bound to that
//! person, which obtain the access and refresh tokens. Those per-user
//! credentials are this installation's alone. No password is ever asked
//! for, and the private API token, which the documentation reserves for
//! personal scripts, is not used.
//!
//! Status checks read `GET /rest/1.0/user`, refreshing the access token
//! first when it has expired or Real-Debrid answers 401. Downloading
//! through Real-Debrid (unrestricting links) is not implemented here.
//!
//! Every stored credential records the API origin it was issued by, and is
//! only ever sent back there.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use url::Url;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, ApiClient, ApiErrorKind, ApiMethod, ApiRequest, ApiResponse, Checked,
    DeviceAuthorization, PollOutcome, Profile, ProviderError,
};
use wayfetch_engine::domain::name::ProviderId;
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_engine::protocol::account::{AccountPlan, AuthorizationMethod, ServiceSupport};
use wayfetch_engine::transfer::writer::BoxFuture;
use zeroize::Zeroize;

/// The service's ID, as in the UI's catalogue.
pub const SERVICE: &str = "real-debrid";
/// The client ID Real-Debrid publishes for open-source applications, used
/// only to obtain per-user credentials.
pub const OPEN_SOURCE_CLIENT_ID: &str = "X245A4XAIBGVM";
/// Real-Debrid's API.
pub const OFFICIAL_API: &str = "https://api.real-debrid.com/";
/// The OAuth grant type of the device flow and of refreshing.
const DEVICE_GRANT: &str = "http://oauth.net/grant_type/device/1.0";
/// Deadline for one request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);
/// Largest response body read.
const MAX_BODY: usize = 64 * 1024;
/// Longest token, code, or secret accepted from Real-Debrid.
const MAX_TOKEN: usize = 2048;
/// Longest user code shown.
const MAX_USER_CODE: usize = 32;
/// An access token this close to expiry is refreshed first.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
/// The stored credential format.
const FORMAT: u8 = 1;

/// Where Real-Debrid's API is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    base: Url,
}

impl Endpoint {
    /// Real-Debrid's own API.
    pub fn official() -> Self {
        Self {
            base: Url::parse(OFFICIAL_API).expect("valid URL"),
        }
    }

    /// Another address serving the same API, such as a local fixture for
    /// testing: HTTPS, or plain HTTP to a loopback address only.
    pub fn custom(base: &str) -> Result<Self, String> {
        let mut base = Url::parse(base).map_err(|e| format!("invalid Real-Debrid address: {e}"))?;
        let loopback = match base.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        let secure = base.scheme() == "https" || base.scheme() == "http" && loopback;
        if !secure
            || base.cannot_be_a_base()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(
                "a Real-Debrid address must be HTTPS, or HTTP on a loopback address, without credentials".into(),
            );
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        base.set_query(None);
        base.set_fragment(None);
        Ok(Self { base })
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("relative API path")
    }

    fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }

    /// Whether `url` is a page a person may be sent to: HTTPS, or this
    /// endpoint's own origin.
    fn allows_page(&self, url: &Url) -> bool {
        url.scheme() == "https" || url.origin() == self.base.origin()
    }
}

/// Real-Debrid accounts.
pub struct RealDebrid {
    client: Arc<dyn ApiClient>,
    endpoint: Endpoint,
    service: ProviderId,
}

impl std::fmt::Debug for RealDebrid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealDebrid")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// Credentials as stored, sealed, by the engine.
#[derive(Serialize, Deserialize)]
struct Stored {
    format: u8,
    origin: String,
    client_id: String,
    client_secret: String,
    access_token: String,
    refresh_token: String,
    /// When the access token expires, in Unix milliseconds.
    expires_at: i64,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.client_secret.zeroize();
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

/// What polling needs.
#[derive(Serialize, Deserialize)]
struct Pending {
    device_code: String,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.device_code.zeroize();
    }
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    interval: u64,
    expires_in: u64,
    verification_url: String,
}

#[derive(Deserialize)]
struct DeviceCredentials {
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct User {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// Seconds of premium left.
    #[serde(default)]
    premium: Option<i64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error_code: Option<i64>,
}

/// Why reading the user failed.
enum UserError {
    /// 401: the access token is not accepted.
    Unauthorized,
    /// 403: permission denied, or the account is locked.
    Forbidden,
    Other(ProviderError),
}

fn now_millis() -> i64 {
    UtcTimestamp::from_system_time(SystemTime::now()).unix_millis()
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn malformed(what: &str) -> ProviderError {
    ProviderError::Malformed(format!("Real-Debrid sent an unexpected answer to {what}"))
}

/// A token, code, or secret Real-Debrid sent, if it is plausible.
fn token(value: String, what: &str) -> Result<String, ProviderError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_TOKEN
        && value.bytes().all(|b| b.is_ascii_graphic());
    if ok { Ok(value) } else { Err(malformed(what)) }
}

fn parse<T: DeserializeOwned>(response: &ApiResponse, what: &str) -> Result<T, ProviderError> {
    serde_json::from_slice(response.body.expose()).map_err(|_| malformed(what))
}

/// Classifies an answer this integration did not expect.
fn unexpected(response: &ApiResponse, what: &str) -> ProviderError {
    let status = response.status;
    if status == 429 || status >= 500 {
        return ProviderError::Unavailable(format!(
            "Real-Debrid answered {what} with HTTP {status}"
        ));
    }
    let code = serde_json::from_slice::<ErrorBody>(response.body.expose())
        .ok()
        .and_then(|e| e.error_code)
        .map(|c| format!(", error code {c}"))
        .unwrap_or_default();
    ProviderError::Malformed(format!(
        "Real-Debrid answered {what} with HTTP {status}{code}"
    ))
}

fn form(fields: &[(&str, &str)]) -> Secret {
    let mut encoded = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        encoded.append_pair(name, value);
    }
    Secret::new(encoded.finish().into_bytes())
}

fn serialize<T: Serialize>(value: &T) -> Secret {
    Secret::new(serde_json::to_vec(value).expect("plain data serializes"))
}

impl RealDebrid {
    /// Real-Debrid accounts through `client`, at `endpoint`.
    pub fn new(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Self {
        Self {
            client,
            endpoint,
            service: ProviderId::new(SERVICE).expect("valid service ID"),
        }
    }

    async fn call(
        &self,
        method: ApiMethod,
        url: Url,
        bearer: Option<&str>,
        form: Option<Secret>,
    ) -> Result<ApiResponse, ProviderError> {
        let request = ApiRequest {
            method,
            url,
            bearer: bearer.map(Secret::from_text),
            form,
            deadline: REQUEST_DEADLINE,
            max_body: MAX_BODY,
        };
        self.client.send(&request).await.map_err(|e| match e.kind {
            ApiErrorKind::Unreachable | ApiErrorKind::Timeout => ProviderError::Unavailable(
                format!("Real-Debrid could not be reached: {}", e.message),
            ),
            ApiErrorKind::TooLarge => malformed("a request: the answer is too large"),
            ApiErrorKind::Refused => ProviderError::Malformed(format!(
                "the request to Real-Debrid was refused: {}",
                e.message
            )),
        })
    }

    fn oauth(&self, path: &str, query: &[(&str, &str)]) -> Url {
        let mut url = self.endpoint.url(&format!("oauth/v2/{path}"));
        url.query_pairs_mut().extend_pairs(query);
        url
    }

    async fn begin(&self) -> Result<DeviceAuthorization, ProviderError> {
        let url = self.oauth(
            "device/code",
            &[
                ("client_id", OPEN_SOURCE_CLIENT_ID),
                ("new_credentials", "yes"),
            ],
        );
        let response = self.call(ApiMethod::Get, url, None, None).await?;
        if response.status != 200 {
            return Err(unexpected(&response, "the sign-in request"));
        }
        let code: DeviceCode = parse(&response, "the sign-in request")?;
        let user_code_ok = !code.user_code.is_empty()
            && code.user_code.len() <= MAX_USER_CODE
            && code
                .user_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let page = Url::parse(&code.verification_url)
            .ok()
            .filter(|url| self.endpoint.allows_page(url));
        let (true, Some(page), 1..=3600) = (user_code_ok, page, code.expires_in) else {
            return Err(malformed("the sign-in request"));
        };
        let device_code = token(code.device_code, "the sign-in request")?;
        Ok(DeviceAuthorization {
            user_code: code.user_code,
            verification_url: page,
            expires_in: Duration::from_secs(code.expires_in),
            interval: Duration::from_secs(code.interval.clamp(1, 60)),
            pending: serialize(&Pending { device_code }),
        })
    }

    async fn poll(&self, pending: &Secret) -> Result<PollOutcome, ProviderError> {
        let pending: Pending = serde_json::from_slice(pending.expose())
            .map_err(|_| ProviderError::Malformed("the sign-in is not in a known format".into()))?;
        let device_code = &pending.device_code;
        let url = self.oauth(
            "device/credentials",
            &[("client_id", OPEN_SOURCE_CLIENT_ID), ("code", device_code)],
        );
        let response = self.call(ApiMethod::Get, url, None, None).await?;
        match response.status {
            200 => {}
            429 => return Ok(PollOutcome::SlowDown),
            s if s >= 500 => return Err(unexpected(&response, "the sign-in")),
            // Real-Debrid answers with an error until the person approves.
            _ => return Ok(PollOutcome::Pending),
        }
        let issued: DeviceCredentials = parse(&response, "the sign-in")?;
        let client_id = token(issued.client_id, "the sign-in")?;
        let client_secret = token(issued.client_secret, "the sign-in")?;
        let token = self
            .token(&client_id, &client_secret, device_code, "the sign-in")
            .await?;
        let mut stored = Stored {
            format: FORMAT,
            origin: self.endpoint.origin(),
            client_id,
            client_secret,
            access_token: String::new(),
            refresh_token: device_code.clone(),
            expires_at: 0,
        };
        apply(&mut stored, token, "the sign-in")?;
        // The account is described if Real-Debrid answers; it is checked
        // again later either way.
        let profile = self.user(&stored.access_token).await.unwrap_or_default();
        Ok(PollOutcome::Authorized {
            credential: serialize(&stored),
            profile,
        })
    }

    /// Exchanges a device code or refresh token for tokens. A refusal means
    /// the sign-in is no longer valid.
    async fn token(
        &self,
        client_id: &str,
        client_secret: &str,
        code: &str,
        what: &str,
    ) -> Result<Token, ProviderError> {
        let body = form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code),
            ("grant_type", DEVICE_GRANT),
        ]);
        let response = self
            .call(ApiMethod::Post, self.oauth("token", &[]), None, Some(body))
            .await?;
        match response.status {
            200 => parse(&response, what),
            400 | 401 | 403 => Err(ProviderError::InvalidCredentials(
                "Real-Debrid no longer accepts this sign-in; sign in again".into(),
            )),
            _ => Err(unexpected(&response, what)),
        }
    }

    async fn user(&self, access_token: &str) -> Result<Profile, UserError> {
        let url = self.endpoint.url("rest/1.0/user");
        let response = self
            .call(ApiMethod::Get, url, Some(access_token), None)
            .await
            .map_err(UserError::Other)?;
        match response.status {
            200 => {}
            401 => return Err(UserError::Unauthorized),
            403 => return Err(UserError::Forbidden),
            _ => return Err(UserError::Other(unexpected(&response, "the account check"))),
        }
        let user: User = parse(&response, "the account check").map_err(UserError::Other)?;
        let plan = match user.kind.as_deref() {
            Some("premium") => Some(AccountPlan::Premium),
            Some("free") => Some(AccountPlan::Free),
            _ => None,
        };
        // Real-Debrid reports the premium time left in seconds; the end is
        // only known when it does.
        let expires_at = match (plan, user.premium) {
            (Some(AccountPlan::Premium), Some(left)) if left > 0 => {
                Some(UtcTimestamp::from_unix_millis(
                    now_millis().saturating_add(left.saturating_mul(1000)),
                ))
            }
            _ => None,
        };
        Ok(Profile {
            user_id: user.id.map(|id| id.to_string()),
            username: user.username,
            plan,
            expires_at,
        })
    }

    async fn refresh(&self, stored: &mut Stored) -> Result<(), ProviderError> {
        let token = self
            .token(
                &stored.client_id,
                &stored.client_secret,
                &stored.refresh_token,
                "the token refresh",
            )
            .await?;
        apply(stored, token, "the token refresh")
    }

    async fn check(&self, credential: &Secret) -> Result<Checked, ProviderError> {
        let mut stored: Stored = serde_json::from_slice(credential.expose())
            .ok()
            .filter(|s: &Stored| s.format == FORMAT)
            .ok_or_else(|| {
                ProviderError::InvalidCredentials(
                    "the stored sign-in is not in a known format; sign in again".into(),
                )
            })?;
        // Credentials only ever go back to the API that issued them.
        if stored.origin != self.endpoint.origin() {
            return Err(ProviderError::InvalidCredentials(
                "the sign-in was issued by a different Real-Debrid address; sign in again".into(),
            ));
        }
        let mut refreshed = false;
        if stored.expires_at <= now_millis().saturating_add(millis(EXPIRY_MARGIN)) {
            self.refresh(&mut stored).await?;
            refreshed = true;
        }
        let profile = loop {
            match self.user(&stored.access_token).await {
                Ok(profile) => break profile,
                Err(UserError::Unauthorized) if !refreshed => {
                    self.refresh(&mut stored).await?;
                    refreshed = true;
                }
                Err(UserError::Unauthorized) => {
                    return Err(ProviderError::InvalidCredentials(
                        "Real-Debrid no longer accepts this sign-in; sign in again".into(),
                    ));
                }
                Err(UserError::Forbidden) => {
                    return Err(ProviderError::InvalidCredentials(
                        "Real-Debrid refused access: the account is locked or not permitted".into(),
                    ));
                }
                Err(UserError::Other(e)) => return Err(e),
            }
        };
        Ok(Checked {
            profile,
            refreshed: refreshed.then(|| serialize(&stored)),
        })
    }
}

/// Takes new tokens; a refresh token is kept unless a new one came.
fn apply(stored: &mut Stored, token: Token, what: &str) -> Result<(), ProviderError> {
    let lifetime = Duration::from_secs(token.expires_in.min(365 * 24 * 3600));
    stored.access_token = self::token(token.access_token, what)?;
    if let Some(refresh) = token.refresh_token {
        stored.refresh_token = self::token(refresh, what)?;
    }
    stored.expires_at = now_millis().saturating_add(millis(lifetime));
    Ok(())
}

impl AccountProvider for RealDebrid {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn support(&self) -> ServiceSupport {
        ServiceSupport {
            service: self.service.clone(),
            method: AuthorizationMethod::DeviceCode,
            downloads: false,
        }
    }

    fn begin(&self) -> BoxFuture<'_, Result<DeviceAuthorization, ProviderError>> {
        Box::pin(self.begin())
    }

    fn poll<'a>(
        &'a self,
        pending: &'a Secret,
    ) -> BoxFuture<'a, Result<PollOutcome, ProviderError>> {
        Box::pin(self.poll(pending))
    }

    fn check<'a>(
        &'a self,
        credential: &'a Secret,
    ) -> BoxFuture<'a, Result<Checked, ProviderError>> {
        Box::pin(self.check(credential))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_endpoints_must_be_secure_or_loopback() {
        assert!(Endpoint::custom("https://rd.example/api").is_ok());
        assert_eq!(
            Endpoint::custom("https://rd.example/api")
                .unwrap()
                .url("rest/1.0/user")
                .as_str(),
            "https://rd.example/api/rest/1.0/user"
        );
        assert!(Endpoint::custom("http://127.0.0.1:9000/").is_ok());
        assert!(Endpoint::custom("http://[::1]:9000/").is_ok());
        for bad in [
            "http://rd.example/",
            "http://192.168.1.2/",
            "https://user:pw@rd.example/",
            "ftp://rd.example/",
            "not a url",
        ] {
            assert!(Endpoint::custom(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn forms_are_encoded_and_secret() {
        let body = form(&[("code", "a b&c"), ("grant_type", DEVICE_GRANT)]);
        assert_eq!(
            body.expose_text().unwrap(),
            "code=a+b%26c&grant_type=http%3A%2F%2Foauth.net%2Fgrant_type%2Fdevice%2F1.0"
        );
        assert_eq!(format!("{body:?}"), "Secret([redacted])");
    }

    #[test]
    fn tokens_must_be_plausible() {
        assert!(token("abc".into(), "x").is_ok());
        assert!(token(String::new(), "x").is_err());
        assert!(token("a b".into(), "x").is_err());
        assert!(token("a".repeat(MAX_TOKEN + 1), "x").is_err());
    }
}
