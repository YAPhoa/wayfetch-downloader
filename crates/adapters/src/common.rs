//! What every integration shares, so each provider module holds only its
//! own wire protocol: where its API is, sending one bounded request
//! through the host's [`ApiClient`], and checking what comes back.
//!
//! Messages name the provider, never a URL, token, or body, so they can
//! reach the UI as they are.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde::de::DeserializeOwned;
use url::Url;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    ApiClient, ApiError, ApiErrorKind, ApiMethod, ApiRequest, ApiResponse, ProviderError,
};
use wayfetch_engine::domain::time::UtcTimestamp;

/// Deadline for one request.
const REQUEST_DEADLINE: Duration = Duration::from_secs(20);
/// Largest response body read.
const MAX_BODY: usize = 64 * 1024;
/// Longest token, code, or secret accepted from a provider.
pub const MAX_TOKEN: usize = 2048;

/// Where a provider's API is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    base: Url,
}

impl Endpoint {
    /// The provider's own API, at `base`.
    pub fn official(base: &str) -> Self {
        Self::custom(base).expect("a valid official API address")
    }

    /// Another address serving the same API, such as a local fixture for
    /// testing: HTTPS, or plain HTTP to a loopback address only.
    pub fn custom(base: &str) -> Result<Self, String> {
        let mut base = Url::parse(base).map_err(|e| format!("invalid API address: {e}"))?;
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
                "an API address must be HTTPS, or HTTP on a loopback address, without credentials"
                    .into(),
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

    /// `path` below the API's address.
    pub fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("relative API path")
    }

    /// The API's origin, which stored credentials record so they are only
    /// ever sent back there.
    pub fn origin(&self) -> String {
        self.base.origin().ascii_serialization()
    }

    /// Whether `url` is a page a person may be sent to: HTTPS, or this
    /// endpoint's own origin.
    pub fn allows_page(&self, url: &Url) -> bool {
        url.scheme() == "https" || url.origin() == self.base.origin()
    }
}

/// One provider's API, through the host's client.
pub struct Api {
    client: Arc<dyn ApiClient>,
    /// The provider's name, for messages.
    name: &'static str,
    endpoint: Endpoint,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

impl Api {
    /// `name`'s API at `endpoint`, through `client`.
    pub fn new(client: Arc<dyn ApiClient>, name: &'static str, endpoint: Endpoint) -> Self {
        Self {
            client,
            name,
            endpoint,
        }
    }

    /// Where the API is.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Sends one bounded request, and says exactly how it failed: whether
    /// the provider may have seen it matters for a request that must not
    /// be repeated.
    pub async fn send(
        &self,
        method: ApiMethod,
        url: Url,
        bearer: Option<&str>,
        form: Option<Secret>,
    ) -> Result<ApiResponse, ApiError> {
        let request = ApiRequest {
            method,
            url,
            bearer: bearer.map(Secret::from_text),
            form,
            deadline: REQUEST_DEADLINE,
            max_body: MAX_BODY,
        };
        self.client.send(&request).await
    }

    /// The provider's name, for messages.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Sends one bounded request. A failure to get any answer is
    /// [`ProviderError::Unavailable`]; any status is an answer.
    pub async fn call(
        &self,
        method: ApiMethod,
        url: Url,
        bearer: Option<&str>,
        form: Option<Secret>,
    ) -> Result<ApiResponse, ProviderError> {
        let name = self.name;
        self.send(method, url, bearer, form)
            .await
            .map_err(|e| match e.kind {
                ApiErrorKind::Unreachable | ApiErrorKind::Timeout | ApiErrorKind::Interrupted => {
                    ProviderError::Unavailable(format!(
                        "{name} could not be reached: {}",
                        e.message
                    ))
                }
                ApiErrorKind::TooLarge => self.malformed("a request: the answer is too large"),
                ApiErrorKind::Refused | ApiErrorKind::Unreadable => ProviderError::Malformed(
                    format!("the request to {name} was refused: {}", e.message),
                ),
            })
    }

    /// An answer to `what` this integration cannot use.
    pub fn malformed(&self, what: &str) -> ProviderError {
        ProviderError::Malformed(format!("{} sent an unexpected answer to {what}", self.name))
    }

    /// The JSON body of an answer to `what`.
    pub fn parse<T: DeserializeOwned>(
        &self,
        response: &ApiResponse,
        what: &str,
    ) -> Result<T, ProviderError> {
        serde_json::from_slice(response.body.expose()).map_err(|_| self.malformed(what))
    }

    /// A token, code, or secret the provider sent, if it is plausible.
    pub fn token(&self, value: String, what: &str) -> Result<String, ProviderError> {
        let ok = !value.is_empty()
            && value.len() <= MAX_TOKEN
            && value.bytes().all(|b| b.is_ascii_graphic());
        if ok {
            Ok(value)
        } else {
            Err(self.malformed(what))
        }
    }

    /// Classifies a status the integration did not expect: rate limits and
    /// server errors are temporary, anything else malformed. `detail` is
    /// the provider's own error code, if it gave one.
    pub fn unexpected(&self, response: &ApiResponse, what: &str, detail: &str) -> ProviderError {
        let (name, status) = (self.name, response.status);
        let message = format!("{name} answered {what} with HTTP {status}{detail}");
        if status == 429 || status >= 500 {
            ProviderError::Unavailable(message)
        } else {
            ProviderError::Malformed(message)
        }
    }
}

/// An `application/x-www-form-urlencoded` body, kept secret.
pub fn form(fields: &[(&str, &str)]) -> Secret {
    let mut encoded = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        encoded.append_pair(name, value);
    }
    Secret::new(encoded.finish().into_bytes())
}

/// `value` as JSON, kept secret: stored credentials and pending sign-ins.
pub fn serialize<T: Serialize>(value: &T) -> Secret {
    Secret::new(serde_json::to_vec(value).expect("plain data serializes"))
}

/// Now, in Unix milliseconds.
pub fn now_millis() -> i64 {
    UtcTimestamp::from_system_time(SystemTime::now()).unix_millis()
}

/// `duration` in milliseconds, saturating.
pub fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_endpoints_must_be_secure_or_loopback() {
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
        let body = form(&[("code", "a b&c"), ("grant", "http://x/y")]);
        assert_eq!(
            body.expose_text().unwrap(),
            "code=a+b%26c&grant=http%3A%2F%2Fx%2Fy"
        );
        assert_eq!(format!("{body:?}"), "Secret([redacted])");
    }

    struct Unused;

    impl ApiClient for Unused {
        fn send<'a>(
            &'a self,
            _: &'a ApiRequest,
        ) -> wayfetch_engine::transfer::writer::BoxFuture<
            'a,
            Result<ApiResponse, wayfetch_engine::accounts::provider::ApiError>,
        > {
            unreachable!("no request is sent")
        }
    }

    #[test]
    fn tokens_must_be_plausible() {
        let api = Api::new(
            Arc::new(Unused),
            "Test",
            Endpoint::official("https://t.example/"),
        );
        assert!(api.token("abc".into(), "x").is_ok());
        assert!(api.token(String::new(), "x").is_err());
        assert!(api.token("a b".into(), "x").is_err());
        assert!(api.token("a".repeat(MAX_TOKEN + 1), "x").is_err());
    }
}
