//! Real-Debrid accounts: sign-in, status, and downloads through them
//! (architecture brief sections 3.11, 6.4, and 8.3).
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
//! through an account is in [`downloads`].
//!
//! Every stored credential records the API origin it was issued by, and is
//! only ever sent back there.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, ApiClient, ApiMethod, ApiResponse, Checked, DeviceAuthorization, PollOutcome,
    Profile, ProviderError, Reconciled, Unrestricted, WithCredentials,
};
use wayfetch_engine::domain::name::ProviderId;
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_engine::protocol::account::{AccountPlan, AuthorizationMethod, ServiceSupport};
use wayfetch_engine::transfer::writer::BoxFuture;
use zeroize::Zeroize;

use crate::common::{Api, Endpoint, form, millis, now_millis, serialize};

mod downloads;

/// The service's ID, as in the UI's catalogue.
pub const SERVICE: &str = "real-debrid";
/// The client ID Real-Debrid publishes for open-source applications, used
/// only to obtain per-user credentials.
pub const OPEN_SOURCE_CLIENT_ID: &str = "X245A4XAIBGVM";
/// Real-Debrid's API.
pub const OFFICIAL_API: &str = "https://api.real-debrid.com/";
/// The OAuth grant type of the device flow and of refreshing.
const DEVICE_GRANT: &str = "http://oauth.net/grant_type/device/1.0";
/// Longest user code shown.
const MAX_USER_CODE: usize = 32;
/// An access token this close to expiry is refreshed first.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
/// The stored credential format.
const FORMAT: u8 = 1;

/// Real-Debrid accounts.
pub struct RealDebrid {
    api: Api,
    service: ProviderId,
}

impl std::fmt::Debug for RealDebrid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealDebrid")
            .field("api", &self.api)
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

impl RealDebrid {
    /// Real-Debrid accounts through `client`, at `endpoint`
    /// ([`OFFICIAL_API`] unless testing).
    pub fn new(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Self {
        Self {
            api: Api::new(client, "Real-Debrid", endpoint),
            service: ProviderId::new(SERVICE).expect("valid service ID"),
        }
    }

    /// [`Self::new`] as a provider, for [`crate::INTEGRATIONS`].
    pub fn provider(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Arc<dyn AccountProvider> {
        Arc::new(Self::new(client, endpoint))
    }

    /// An answer this integration did not expect, with Real-Debrid's own
    /// error code if it gave one.
    fn unexpected(&self, response: &ApiResponse, what: &str) -> ProviderError {
        let code = serde_json::from_slice::<ErrorBody>(response.body.expose())
            .ok()
            .and_then(|e| e.error_code)
            .map(|c| format!(", error code {c}"))
            .unwrap_or_default();
        self.api.unexpected(response, what, &code)
    }

    fn oauth(&self, path: &str, query: &[(&str, &str)]) -> Url {
        let mut url = self.api.endpoint().url(&format!("oauth/v2/{path}"));
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
        let response = self.api.call(ApiMethod::Get, url, None, None).await?;
        if response.status != 200 {
            return Err(self.unexpected(&response, "the sign-in request"));
        }
        let code: DeviceCode = self.api.parse(&response, "the sign-in request")?;
        let user_code_ok = !code.user_code.is_empty()
            && code.user_code.len() <= MAX_USER_CODE
            && code
                .user_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        let page = Url::parse(&code.verification_url)
            .ok()
            .filter(|url| self.api.endpoint().allows_page(url));
        let (true, Some(page), 1..=3600) = (user_code_ok, page, code.expires_in) else {
            return Err(self.api.malformed("the sign-in request"));
        };
        let device_code = self.api.token(code.device_code, "the sign-in request")?;
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
        let response = self.api.call(ApiMethod::Get, url, None, None).await?;
        match response.status {
            200 => {}
            429 => return Ok(PollOutcome::SlowDown),
            s if s >= 500 => return Err(self.unexpected(&response, "the sign-in")),
            // Real-Debrid answers with an error until the person approves.
            _ => return Ok(PollOutcome::Pending),
        }
        let issued: DeviceCredentials = self.api.parse(&response, "the sign-in")?;
        let client_id = self.api.token(issued.client_id, "the sign-in")?;
        let client_secret = self.api.token(issued.client_secret, "the sign-in")?;
        let token = self
            .token(&client_id, &client_secret, device_code, "the sign-in")
            .await?;
        let mut stored = Stored {
            format: FORMAT,
            origin: self.api.endpoint().origin(),
            client_id,
            client_secret,
            access_token: String::new(),
            refresh_token: device_code.clone(),
            expires_at: 0,
        };
        self.apply(&mut stored, token, "the sign-in")?;
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
            .api
            .call(ApiMethod::Post, self.oauth("token", &[]), None, Some(body))
            .await?;
        match response.status {
            200 => self.api.parse(&response, what),
            400 | 401 | 403 => Err(ProviderError::InvalidCredentials(
                "Real-Debrid no longer accepts this sign-in; sign in again".into(),
            )),
            _ => Err(self.unexpected(&response, what)),
        }
    }

    async fn user(&self, access_token: &str) -> Result<Profile, UserError> {
        let url = self.api.endpoint().url("rest/1.0/user");
        let response = self
            .api
            .call(ApiMethod::Get, url, Some(access_token), None)
            .await
            .map_err(UserError::Other)?;
        match response.status {
            200 => {}
            401 => return Err(UserError::Unauthorized),
            403 => return Err(UserError::Forbidden),
            _ => {
                return Err(UserError::Other(
                    self.unexpected(&response, "the account check"),
                ));
            }
        }
        let user: User = self
            .api
            .parse(&response, "the account check")
            .map_err(UserError::Other)?;
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
        self.apply(stored, token, "the token refresh")
    }

    async fn check(&self, credential: &Secret) -> Checked {
        let stored = serde_json::from_slice(credential.expose())
            .ok()
            .filter(|s: &Stored| s.format == FORMAT);
        let Some(mut stored) = stored else {
            return Checked::failed(ProviderError::InvalidCredentials(
                "the stored sign-in is not in a known format; sign in again".into(),
            ));
        };
        let mut refreshed = false;
        let result = self.confirm(&mut stored, &mut refreshed).await;
        // New tokens are kept even when the account could not be confirmed
        // afterwards: Real-Debrid may no longer accept the old ones.
        Checked {
            result,
            refreshed: refreshed.then(|| serialize(&stored)),
        }
    }

    /// Reads the account, refreshing the access token first when it has
    /// expired or Real-Debrid no longer accepts it. `refreshed` says
    /// whether `stored` now holds new tokens.
    async fn confirm(
        &self,
        stored: &mut Stored,
        refreshed: &mut bool,
    ) -> Result<Profile, ProviderError> {
        // Credentials only ever go back to the API that issued them.
        if stored.origin != self.api.endpoint().origin() {
            return Err(ProviderError::InvalidCredentials(
                "the sign-in was issued by a different Real-Debrid address; sign in again".into(),
            ));
        }
        if stored.expires_at <= now_millis().saturating_add(millis(EXPIRY_MARGIN)) {
            self.refresh(stored).await?;
            *refreshed = true;
        }
        loop {
            match self.user(&stored.access_token).await {
                Ok(profile) => return Ok(profile),
                Err(UserError::Unauthorized) if !*refreshed => {
                    self.refresh(stored).await?;
                    *refreshed = true;
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
        }
    }

    /// Takes new tokens, all or none; a refresh token is kept unless a new
    /// one came.
    fn apply(&self, stored: &mut Stored, token: Token, what: &str) -> Result<(), ProviderError> {
        let lifetime = Duration::from_secs(token.expires_in.min(365 * 24 * 3600));
        let access = self.api.token(token.access_token, what)?;
        let refresh = token
            .refresh_token
            .map(|refresh| self.api.token(refresh, what))
            .transpose()?;
        stored.access_token.zeroize();
        stored.access_token = access;
        if let Some(refresh) = refresh {
            stored.refresh_token.zeroize();
            stored.refresh_token = refresh;
        }
        stored.expires_at = now_millis().saturating_add(millis(lifetime));
        Ok(())
    }
}

impl AccountProvider for RealDebrid {
    fn service(&self) -> &ProviderId {
        &self.service
    }

    fn support(&self) -> ServiceSupport {
        ServiceSupport {
            service: self.service.clone(),
            method: AuthorizationMethod::DeviceCode,
            downloads: true,
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

    fn check<'a>(&'a self, credential: &'a Secret) -> BoxFuture<'a, Checked> {
        Box::pin(self.check(credential))
    }

    fn unrestrict<'a>(
        &'a self,
        credential: &'a Secret,
        link: &'a Url,
    ) -> BoxFuture<'a, WithCredentials<Unrestricted>> {
        Box::pin(self.unrestrict(credential, link))
    }

    fn find_generated<'a>(
        &'a self,
        credential: &'a Secret,
        link: &'a Url,
        since: UtcTimestamp,
    ) -> BoxFuture<'a, WithCredentials<Reconciled>> {
        Box::pin(self.find_generated(credential, link, since))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_grant_is_form_encoded() {
        let body = form(&[("code", "a b&c"), ("grant_type", DEVICE_GRANT)]);
        assert_eq!(
            body.expose_text().unwrap(),
            "code=a+b%26c&grant_type=http%3A%2F%2Foauth.net%2Fgrant_type%2Fdevice%2F1.0"
        );
    }
}
