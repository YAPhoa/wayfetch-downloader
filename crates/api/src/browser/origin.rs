//! Which browser origins may use the API.
//!
//! A browser request must use a configured `Host`, which defeats DNS
//! rebinding, and come from a configured origin per `Sec-Fetch-Site` and
//! `Origin`. No CORS headers are ever sent.

use axum::http::{HeaderMap, Method, header};
use url::{Host, Url};
use wayfetch_engine::protocol::ErrorCode;

use super::BrowserAuthError;
use crate::error::ApiError;

/// The origins browsers may use.
#[derive(Debug)]
pub(super) struct OriginPolicy {
    /// Serialized origins, exactly as browsers send them in `Origin`.
    origins: Vec<String>,
    /// `Host` header values that reach this server under those origins.
    hosts: Vec<String>,
}

impl OriginPolicy {
    /// The policy for `origins`. Each must be HTTPS, or, only in the
    /// loopback development profile, and then all of them, plain HTTP on
    /// a loopback host.
    pub(super) fn new(origins: &[Url], insecure_loopback: bool) -> Result<Self, BrowserAuthError> {
        if origins.is_empty() {
            return Err(BrowserAuthError::NoOrigin);
        }
        let mut policy = Self {
            origins: Vec::new(),
            hosts: Vec::new(),
        };
        for url in origins {
            let bad = |reason| BrowserAuthError::BadOrigin {
                origin: url.to_string(),
                reason,
            };
            if !url.username().is_empty()
                || url.password().is_some()
                || url.path() != "/"
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(bad(
                    "an origin is a scheme, a host, and an optional port only",
                ));
            }
            let host = url.host().ok_or_else(|| bad("an origin needs a host"))?;
            match (url.scheme(), insecure_loopback) {
                ("https", false) => {}
                ("http", true) if is_loopback(&host) => {}
                ("http", false) => {
                    return Err(bad(
                        "plain HTTP is allowed only for loopback development; use HTTPS",
                    ));
                }
                (_, true) => {
                    return Err(bad(
                        "loopback development allows only plain-HTTP loopback origins",
                    ));
                }
                _ => return Err(bad("the scheme must be https")),
            }
            policy.origins.push(url.origin().ascii_serialization());
            let host = host.to_string();
            match (url.port(), url.port_or_known_default()) {
                (Some(port), _) => policy.hosts.push(format!("{host}:{port}")),
                // Browsers leave a default port out of `Host`; other
                // clients may not.
                (None, Some(port)) => {
                    policy.hosts.push(format!("{host}:{port}"));
                    policy.hosts.push(host);
                }
                (None, None) => policy.hosts.push(host),
            }
        }
        Ok(policy)
    }

    /// The origins, as browsers send them.
    pub(super) fn origins(&self) -> &[String] {
        &self.origins
    }

    /// Refuses a browser request that did not come from, or did not
    /// address, one of the origins. A request that changes state must
    /// name its origin.
    pub(super) fn check(&self, method: &Method, headers: &HeaderMap) -> Result<(), ApiError> {
        let refuse = || ApiError::new(ErrorCode::PermissionDenied, "cross-origin request refused");
        let host = headers
            .get(header::HOST)
            .and_then(|host| host.to_str().ok())
            .ok_or_else(refuse)?;
        if !self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
            return Err(refuse());
        }
        // Fetch metadata, where the browser sends it: `none` is the user
        // opening a URL directly.
        if let Some(site) = headers.get("sec-fetch-site")
            && !matches!(site.as_bytes(), b"same-origin" | b"none")
        {
            return Err(refuse());
        }
        match headers.get(header::ORIGIN) {
            Some(origin)
                if self
                    .origins
                    .iter()
                    .any(|o| o.as_bytes() == origin.as_bytes()) =>
            {
                Ok(())
            }
            None if method.is_safe() => Ok(()),
            _ => Err(refuse()),
        }
    }
}

/// Whether `host` names this machine's loopback interface.
pub(super) fn is_loopback(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn policy(origins: &[&str], insecure: bool) -> Result<OriginPolicy, BrowserAuthError> {
        let urls = origins
            .iter()
            .map(|o| Url::parse(o).unwrap())
            .collect::<Vec<_>>();
        OriginPolicy::new(&urls, insecure)
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    header::HeaderName::from_static(k),
                    HeaderValue::from_static(v),
                )
            })
            .collect()
    }

    #[test]
    fn only_https_origins_unless_loopback_development() {
        assert!(policy(&["https://wayfetch.example.lan"], false).is_ok());
        assert!(policy(&["https://10.0.0.5:8443"], false).is_ok());
        assert!(policy(&["http://127.0.0.1:7878", "http://localhost:7878"], true).is_ok());
        assert!(policy(&["http://[::1]:7878"], true).is_ok());
        for (origin, insecure) in [
            ("http://127.0.0.1:7878", false),
            ("http://wayfetch.example.lan", false),
            ("http://192.168.1.2:7878", true),
            ("https://127.0.0.1:7878", true),
            ("https://wayfetch.example.lan/app", false),
            ("https://user@wayfetch.example.lan", false),
            ("https://wayfetch.example.lan/?x=1", false),
            ("ftp://wayfetch.example.lan", false),
        ] {
            assert!(
                matches!(
                    policy(&[origin], insecure),
                    Err(BrowserAuthError::BadOrigin { .. })
                ),
                "{origin}"
            );
        }
        assert!(matches!(
            policy(&[], false),
            Err(BrowserAuthError::NoOrigin)
        ));
    }

    #[test]
    fn requests_must_address_and_come_from_an_origin() {
        let p = policy(&["https://Wayfetch.Example.lan"], false).unwrap();
        assert_eq!(p.origins(), ["https://wayfetch.example.lan"]);
        let post = Method::POST;
        let get = Method::GET;
        let ok = [
            ("host", "wayfetch.example.lan"),
            ("origin", "https://wayfetch.example.lan"),
        ];
        assert!(p.check(&post, &headers(&ok)).is_ok());
        let ok_port = [
            ("host", "wayfetch.example.lan:443"),
            ("origin", "https://wayfetch.example.lan"),
            ("sec-fetch-site", "same-origin"),
        ];
        assert!(p.check(&post, &headers(&ok_port)).is_ok());
        // A navigation or a same-origin GET may leave `Origin` out.
        assert!(
            p.check(&get, &headers(&[("host", "wayfetch.example.lan")]))
                .is_ok()
        );
        assert!(
            p.check(
                &get,
                &headers(&[("host", "wayfetch.example.lan"), ("sec-fetch-site", "none")])
            )
            .is_ok()
        );

        for bad in [
            // Rebound DNS: right origin, wrong host.
            &[
                ("host", "evil.test"),
                ("origin", "https://wayfetch.example.lan"),
            ][..],
            &[("origin", "https://wayfetch.example.lan")][..],
            &[
                ("host", "wayfetch.example.lan"),
                ("origin", "https://evil.test"),
            ][..],
            &[("host", "wayfetch.example.lan"), ("origin", "null")][..],
            &[
                ("host", "wayfetch.example.lan"),
                ("origin", "http://wayfetch.example.lan"),
            ][..],
            &[
                ("host", "wayfetch.example.lan"),
                ("origin", "https://wayfetch.example.lan"),
                ("sec-fetch-site", "cross-site"),
            ][..],
            &[
                ("host", "wayfetch.example.lan"),
                ("origin", "https://wayfetch.example.lan"),
                ("sec-fetch-site", "same-site"),
            ][..],
            // A state change must say where it came from.
            &[("host", "wayfetch.example.lan")][..],
        ] {
            assert!(p.check(&post, &headers(bad)).is_err(), "{bad:?}");
        }
        assert!(
            p.check(
                &get,
                &headers(&[
                    ("host", "wayfetch.example.lan"),
                    ("sec-fetch-site", "cross-site")
                ])
            )
            .is_err()
        );
    }
}
