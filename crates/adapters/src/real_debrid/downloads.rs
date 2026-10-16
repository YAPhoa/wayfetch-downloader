//! Downloading through a Real-Debrid account: generating a download link
//! for a hoster link, and reading the account's record of generated links
//! (<https://api.real-debrid.com/>, sections "unrestrict" and "downloads").
//!
//! `POST /unrestrict/link` is not idempotent, so it is sent once per call:
//! a lost answer is reported as ambiguous for the engine to reconcile, never
//! resent. Only a 401 before Real-Debrid processed anything (an expired
//! access token) is answered by refreshing and sending once more. The
//! engine reconciles with `GET /downloads`, the account's list of generated
//! links, newest first. That list is the account's history, not a record
//! of operations, so an entry is accepted only if it names exactly the
//! original link, is dated no earlier than the intent, and is the only one
//! that might be this request's; anything less certain is unknown. Torrents, folders, passwords, Remote traffic, and
//! multi-result links are not used.
//!
//! The generated link is checked before the engine sees it: HTTPS (plain
//! HTTP only to a loopback test server), without credentials, of bounded
//! length. Real-Debrid's file name and size are hints; a size of 0 means
//! unknown.

use std::time::Duration;

use serde::Deserialize;
use url::Url;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    ApiErrorKind, ApiMethod, ApiResponse, GeneratedLink, ProviderError, Reconciled, Unrestricted,
    WithCredentials,
};
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::time::UtcTimestamp;
use zeroize::Zeroize;

use super::{ErrorBody, FORMAT, RealDebrid, Stored};
use crate::common::{form, millis, now_millis, serialize};

/// Longest generated link accepted.
const MAX_LINK: usize = 4096;
/// Longest file name hint kept.
const MAX_NAME: usize = 255;
/// Generated links read when reconciling: Real-Debrid lists the newest
/// first, and a lost answer is recent.
const RECONCILE_PAGE: u32 = 100;
/// How long Real-Debrid asks to wait after a rate limit, when it does not
/// say (its documented limit is 250 requests a minute).
const SLOW_DOWN: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct Unrestrict {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    filesize: Option<u64>,
    #[serde(default)]
    download: Option<String>,
    /// Present on multi-result answers (several qualities of one video).
    #[serde(default)]
    alternative: Option<serde_json::Value>,
}

impl Drop for Unrestrict {
    fn drop(&mut self) {
        if let Some(download) = &mut self.download {
            download.zeroize();
        }
    }
}

#[derive(Deserialize)]
struct Generated {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    filesize: Option<u64>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    download: Option<String>,
    #[serde(default)]
    generated: Option<String>,
}

impl Drop for Generated {
    fn drop(&mut self) {
        if let Some(download) = &mut self.download {
            download.zeroize();
        }
    }
}

/// Why the credentials could not be made current before a request.
enum Ready {
    Ok,
    Refused(Unrestricted),
}

impl RealDebrid {
    /// The stored credentials, if readable and issued by this API;
    /// otherwise what the person can do about it.
    fn stored(&self, credential: &Secret) -> Result<Stored, &'static str> {
        let stored = serde_json::from_slice(credential.expose())
            .ok()
            .filter(|s: &Stored| s.format == FORMAT)
            .ok_or("The account's stored sign-in is not in a known format. Sign it in again.")?;
        if stored.origin != self.api.endpoint().origin() {
            return Err(
                "The account was signed in at a different Real-Debrid address. Sign it in again.",
            );
        }
        Ok(stored)
    }

    /// Refreshes the access token, mapping a failure to a refusal: nothing
    /// was asked of Real-Debrid's downloads yet.
    async fn refresh_for_request(&self, stored: &mut Stored) -> Ready {
        match self.refresh(stored).await {
            Ok(()) => Ready::Ok,
            Err(ProviderError::InvalidCredentials(why)) => Ready::Refused(Unrestricted::refused(
                FailureCode::InvalidCredentials,
                format!("{why}."),
            )),
            Err(ProviderError::Unavailable(why) | ProviderError::Malformed(why)) => Ready::Refused(
                Unrestricted::refused(FailureCode::RequestTimeout, format!("{why}. Retry later.")),
            ),
        }
    }

    /// Refreshes the access token first if it has expired.
    async fn fresh(&self, stored: &mut Stored, refreshed: &mut bool) -> Ready {
        if stored.expires_at <= now_millis().saturating_add(millis(super::EXPIRY_MARGIN)) {
            if let Ready::Refused(why) = self.refresh_for_request(stored).await {
                return Ready::Refused(why);
            }
            *refreshed = true;
        }
        Ready::Ok
    }

    /// Asks Real-Debrid once for a download link for `link`.
    pub(super) async fn unrestrict(
        &self,
        credential: &Secret,
        link: &Url,
    ) -> WithCredentials<Unrestricted> {
        let mut stored = match self.stored(credential) {
            Ok(stored) => stored,
            Err(why) => {
                return WithCredentials::unchanged(Unrestricted::refused(
                    FailureCode::InvalidCredentials,
                    why,
                ));
            }
        };
        let mut refreshed = false;
        let result = self.ask(&mut stored, &mut refreshed, link).await;
        WithCredentials {
            result,
            refreshed: refreshed.then(|| serialize(&stored)),
        }
    }

    async fn ask(&self, stored: &mut Stored, refreshed: &mut bool, link: &Url) -> Unrestricted {
        if let Ready::Refused(why) = self.fresh(stored, refreshed).await {
            return why;
        }
        let url = self.api.endpoint().url("rest/1.0/unrestrict/link");
        loop {
            let body = form(&[("link", link.as_str())]);
            let sent = self
                .api
                .send(
                    ApiMethod::Post,
                    url.clone(),
                    Some(&stored.access_token),
                    Some(body),
                )
                .await;
            let response = match sent {
                Ok(response) => response,
                // Never sent: refused before sending, or no connection.
                Err(e) if matches!(e.kind, ApiErrorKind::Unreachable | ApiErrorKind::Refused) => {
                    return Unrestricted::refused(
                        FailureCode::RequestTimeout,
                        format!(
                            "Real-Debrid could not be reached ({}). Retry later.",
                            e.message
                        ),
                    );
                }
                // Sent, and answered unreadably or not at all (a redirect,
                // an encoded or oversized body, a timeout, a lost
                // connection): it may have generated the link.
                Err(e) => {
                    return Unrestricted::Ambiguous(format!(
                        "Real-Debrid's answer was lost ({})",
                        e.message
                    ));
                }
            };
            if response.status == 401 && !*refreshed {
                // Not processed: the token was refused first.
                if let Ready::Refused(why) = self.refresh_for_request(stored).await {
                    return why;
                }
                *refreshed = true;
                continue;
            }
            return self.unrestricted(&response);
        }
    }

    /// Reads Real-Debrid's answer to an unrestrict request.
    fn unrestricted(&self, response: &ApiResponse) -> Unrestricted {
        if response.status == 200 {
            let Ok(answer) = serde_json::from_slice::<Unrestrict>(response.body.expose()) else {
                return Unrestricted::Ambiguous(
                    "Real-Debrid answered, but its answer could not be read".into(),
                );
            };
            if answer.alternative.as_ref().is_some_and(|a| !a.is_null()) {
                return Unrestricted::refused(
                    FailureCode::UnsupportedCapability,
                    "Real-Debrid offers several files for this link, which Wayfetch cannot \
                     choose between yet.",
                );
            }
            let link = answer
                .download
                .as_deref()
                .and_then(|d| self.generated(d, answer.filename.as_deref(), answer.filesize));
            return match link {
                Some(link) => Unrestricted::Generated(link),
                None => Unrestricted::refused(
                    FailureCode::AdapterParseFailure,
                    "Real-Debrid generated a link Wayfetch will not use.",
                ),
            };
        }
        let code = serde_json::from_slice::<ErrorBody>(response.body.expose())
            .ok()
            .and_then(|e| e.error_code);
        refusal(response.status, code)
    }

    /// A generated link, if it is one Wayfetch will use.
    fn generated(
        &self,
        download: &str,
        filename: Option<&str>,
        filesize: Option<u64>,
    ) -> Option<GeneratedLink> {
        if download.len() > MAX_LINK {
            return None;
        }
        let url = Url::parse(download).ok()?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        // A loopback link only from a loopback test API.
        let local = self.api.endpoint().url("").host().is_some_and(|h| match h {
            url::Host::Ipv4(ip) => ip.is_loopback(),
            url::Host::Ipv6(ip) => ip.is_loopback(),
            url::Host::Domain(_) => false,
        });
        let secure = url.scheme() == "https" || url.scheme() == "http" && loopback && local;
        if !secure || url.username() != "" || url.password().is_some() || url.host().is_none() {
            return None;
        }
        Some(GeneratedLink {
            url,
            filename: filename
                .filter(|n| !n.is_empty() && n.len() <= MAX_NAME)
                .map(str::to_owned),
            size: filesize.filter(|&s| s > 0),
        })
    }

    /// Looks through the account's newest generated links for one made for
    /// `link` at or after `since`.
    pub(super) async fn find_generated(
        &self,
        credential: &Secret,
        link: &Url,
        since: UtcTimestamp,
    ) -> WithCredentials<Reconciled> {
        let mut stored = match self.stored(credential) {
            Ok(stored) => stored,
            Err(_) => {
                return WithCredentials::unchanged(Reconciled::Unknown(
                    "the account's sign-in could not be read".into(),
                ));
            }
        };
        let mut refreshed = false;
        let result = self.look(&mut stored, &mut refreshed, link, since).await;
        WithCredentials {
            result,
            refreshed: refreshed.then(|| serialize(&stored)),
        }
    }

    async fn look(
        &self,
        stored: &mut Stored,
        refreshed: &mut bool,
        link: &Url,
        since: UtcTimestamp,
    ) -> Reconciled {
        if let Ready::Refused(_) = self.fresh(stored, refreshed).await {
            return Reconciled::Unknown("the account's sign-in could not be refreshed".into());
        }
        let mut url = self.api.endpoint().url("rest/1.0/downloads");
        url.query_pairs_mut()
            .append_pair("limit", &RECONCILE_PAGE.to_string());
        loop {
            // A read: repeating it changes nothing.
            let response = match self
                .api
                .call(
                    ApiMethod::Get,
                    url.clone(),
                    Some(&stored.access_token),
                    None,
                )
                .await
            {
                Ok(response) => response,
                Err(e) => return Reconciled::Unknown(e.to_string()),
            };
            match response.status {
                200 => {}
                // Real-Debrid answers an empty list with 204.
                204 => return Reconciled::NotFound,
                401 if !*refreshed => {
                    if let Ready::Refused(_) = self.refresh_for_request(stored).await {
                        return Reconciled::Unknown(
                            "the account's sign-in could not be refreshed".into(),
                        );
                    }
                    *refreshed = true;
                    continue;
                }
                status => {
                    return Reconciled::Unknown(format!(
                        "Real-Debrid answered the record with HTTP {status}"
                    ));
                }
            }
            let Ok(entries) = serde_json::from_slice::<Vec<Generated>>(response.body.expose())
            else {
                return Reconciled::Unknown("Real-Debrid's record could not be read".into());
            };
            let mut oldest = None;
            let mut found = Vec::new();
            let mut unclear = false;
            for entry in &entries {
                let at = entry.generated.as_deref().and_then(parse_rfc3339);
                oldest = match (oldest, at) {
                    (Some(o), Some(a)) => Some(std::cmp::min(o, a)),
                    (None, a) => a,
                    (o, None) => o,
                };
                // Older than the intent: another request's.
                if at.is_some_and(|at| at < since.unix_millis()) {
                    continue;
                }
                let recorded = entry.link.as_deref().and_then(|l| Url::parse(l).ok());
                match recorded.as_ref().map(|r| identity(r, link)) {
                    Some(Identity::Other) => {}
                    Some(Identity::Same) if at.is_some() => {
                        match entry.download.as_deref().and_then(|d| {
                            self.generated(d, entry.filename.as_deref(), entry.filesize)
                        }) {
                            Some(generated) => found.push(generated),
                            None => unclear = true,
                        }
                    }
                    // Undated, unreadable, or a different form of the
                    // link (another scheme, port, or query): it may be
                    // this request's, and it may not.
                    _ => unclear = true,
                }
            }
            if unclear {
                return Reconciled::Inconclusive(
                    "Real-Debrid's record lists a link that may or may not be this download's"
                        .into(),
                );
            }
            if found.len() > 1 {
                return Reconciled::Inconclusive(
                    "Real-Debrid's record lists several links for this download, and Wayfetch \
                     cannot tell which is its own"
                        .into(),
                );
            }
            // A full page that does not reach back to the intent proves
            // nothing about older entries: one of them may match too, so
            // even a single visible match is not unique.
            let complete = entries.len() < RECONCILE_PAGE as usize
                || oldest.is_some_and(|o| o < since.unix_millis());
            if !complete {
                return Reconciled::Inconclusive(
                    "Real-Debrid generated too many links since to check".into(),
                );
            }
            return match found.pop() {
                Some(generated) => Reconciled::Found(generated),
                None => Reconciled::NotFound,
            };
        }
    }
}

/// How an original link in Real-Debrid's record relates to `link`.
#[derive(Debug, PartialEq, Eq)]
enum Identity {
    /// Exactly `link`: the same scheme, host, port, path, and query.
    Same,
    /// The same host and path in another form, such as another scheme,
    /// port, query, or a trailing slash: proof neither way.
    Similar,
    /// Another link.
    Other,
}

fn identity(recorded: &Url, link: &Url) -> Identity {
    let near = |u: &Url| {
        (
            u.host_str().map(str::to_ascii_lowercase),
            u.path().trim_end_matches('/').to_owned(),
        )
    };
    if recorded == link {
        Identity::Same
    } else if near(recorded) == near(link) {
        Identity::Similar
    } else {
        Identity::Other
    }
}

/// What an error answer from `POST /unrestrict/link` means. Every code
/// here is a refusal: Real-Debrid generated nothing. A server error
/// without a known code, 503 included, may have happened after it did, so
/// it is ambiguous.
fn refusal(status: u16, code: Option<i64>) -> Unrestricted {
    use FailureCode::*;
    let (failure, message) = match code {
        Some(5 | 34) => {
            return Unrestricted::Refused {
                code: QuotaExhausted,
                message: "Real-Debrid asked Wayfetch to slow down. Retry in a minute.".into(),
                retry_after: Some(SLOW_DOWN),
            };
        }
        Some(8) => (
            InvalidCredentials,
            "Real-Debrid no longer accepts this account's sign-in. Sign it in again.",
        ),
        Some(9) => (
            InvalidCredentials,
            "Real-Debrid refused this account permission to download.",
        ),
        Some(14) => (
            InvalidCredentials,
            "Real-Debrid says this account is locked. Check it on Real-Debrid's site.",
        ),
        Some(15) => (
            InvalidCredentials,
            "Real-Debrid says this account is not activated yet.",
        ),
        Some(22) => (
            InvalidCredentials,
            "Real-Debrid does not allow downloads from this IP address for this account.",
        ),
        Some(20) => (
            HostUnavailableViaProvider,
            "Real-Debrid serves this host to premium accounts only.",
        ),
        Some(16) => (
            HostUnavailableViaProvider,
            "Real-Debrid does not support this host.",
        ),
        Some(17) => (
            HostUnavailableViaProvider,
            "Real-Debrid's support for this host is under maintenance. Retry later.",
        ),
        Some(18) => (
            HostUnavailableViaProvider,
            "This account reached Real-Debrid's limit for this host. Retry later.",
        ),
        Some(19) => (
            HostUnavailableViaProvider,
            "Real-Debrid cannot reach this host right now. Retry later.",
        ),
        Some(21) => (
            QuotaExhausted,
            "This account has too many active downloads at Real-Debrid. Retry once one ends.",
        ),
        Some(23) => (
            QuotaExhausted,
            "This account's Real-Debrid traffic is exhausted.",
        ),
        Some(36) => (
            QuotaExhausted,
            "This account reached Real-Debrid's fair-usage limit.",
        ),
        Some(24) => (
            ResourceDeleted,
            "Real-Debrid says the file is unavailable at its host.",
        ),
        Some(28 | 35) => (ResourceDeleted, "Real-Debrid will not serve this file."),
        Some(2) => (
            AdapterParseFailure,
            "Real-Debrid did not accept this link. Check that it is a file link.",
        ),
        Some(25) => (
            RequestTimeout,
            "Real-Debrid is temporarily unavailable. Retry later.",
        ),
        Some(37) => (
            UnsupportedCapability,
            "Real-Debrid has disabled link generation for now.",
        ),
        _ if status == 429 => {
            return Unrestricted::Refused {
                code: QuotaExhausted,
                message: "Real-Debrid asked Wayfetch to slow down. Retry in a minute.".into(),
                retry_after: Some(SLOW_DOWN),
            };
        }
        // Received, and failed without saying why: it may have generated
        // the link first.
        _ if status >= 500 => {
            return Unrestricted::Ambiguous(format!(
                "Real-Debrid failed with HTTP {status} after receiving the request"
            ));
        }
        Some(code) => {
            return Unrestricted::refused(
                AdapterParseFailure,
                format!("Real-Debrid refused the link (HTTP {status}, error code {code})."),
            );
        }
        None => {
            return Unrestricted::refused(
                AdapterParseFailure,
                format!("Real-Debrid refused the link (HTTP {status})."),
            );
        }
    };
    Unrestricted::refused(failure, message)
}

/// Unix milliseconds of an RFC 3339 date and time, such as Real-Debrid's
/// `2026-10-16T10:00:00.000Z`. `None` if it is not one.
fn parse_rfc3339(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        if end == 0 {
            return None;
        }
        let first: String = fraction[..end]
            .chars()
            .chain("000".chars())
            .take(3)
            .collect();
        millis = first.parse().ok()?;
        rest = &fraction[end..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && matches!(&rest[..1], "+" | "-") && &rest[3..4] == ":" => {
            let h: i64 = rest[1..3].parse().ok()?;
            let m: i64 = rest[4..6].parse().ok()?;
            let sign = if &rest[..1] == "-" { -1 } else { 1 };
            sign * (h * 3600 + m * 60)
        }
        _ => return None,
    };
    // Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(seconds * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_parse_as_rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-16T10:00:00.250Z"),
            Some(1_792_144_800_250)
        );
        assert_eq!(
            parse_rfc3339("2026-10-16T12:00:00+02:00"),
            Some(1_792_144_800_000)
        );
        for bad in [
            "",
            "2026-10-16",
            "2026-13-01T00:00:00Z",
            "2026-10-16T10:00:00",
            "x",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn every_documented_refusal_generated_nothing() {
        for code in [
            2, 5, 8, 9, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 28, 34, 35, 36, 37,
        ] {
            assert!(
                matches!(refusal(403, Some(code)), Unrestricted::Refused { .. }),
                "{code}"
            );
        }
        assert!(matches!(refusal(500, None), Unrestricted::Ambiguous(_)));
        assert!(matches!(
            refusal(429, None),
            Unrestricted::Refused {
                code: FailureCode::QuotaExhausted,
                retry_after: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn only_the_exact_link_is_the_same_source() {
        let link = Url::parse("https://host.example/file/abc?x=1").unwrap();
        let same = Url::parse("https://HOST.example:443/file/abc?x=1").unwrap();
        assert_eq!(identity(&same, &link), Identity::Same);
        for similar in [
            "https://host.example/file/abc/?x=1",
            "http://host.example/file/abc?x=1",
            "https://host.example:8443/file/abc?x=1",
            "https://host.example/file/abc",
        ] {
            let recorded = Url::parse(similar).unwrap();
            assert_eq!(identity(&recorded, &link), Identity::Similar, "{similar}");
        }
        for other in [
            "https://host.example/file/abd?x=1",
            "https://other.example/file/abc?x=1",
        ] {
            let recorded = Url::parse(other).unwrap();
            assert_eq!(identity(&recorded, &link), Identity::Other, "{other}");
        }
    }
}
