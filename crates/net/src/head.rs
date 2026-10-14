//! Reading a response's status and headers into what the engine checks:
//! framing, representation, and a suggested file name (sections 7.5 and
//! 8.2; tests T02 to T04, T07, and T08).
//!
//! Parsing is strict: a malformed `Content-Range` or `Content-Length` is a
//! failure, never a guess, and an encoded body is refused rather than
//! decoded, so byte offsets always address the stored representation.

use std::fmt::Write as _;
use std::time::SystemTime;

use percent_encoding::percent_decode_str;
use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap};
use sha2::{Digest, Sha256};
use url::Url;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::TransportError;

/// What a response's status and headers say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    /// How the status and `Content-Range` frame the body.
    pub framing: Framing,
    /// What the response says about the representation.
    pub representation: Representation,
    /// The file name `Content-Disposition` suggested, if any.
    pub filename: Option<String>,
}

pub(crate) fn error(code: FailureCode, message: impl Into<String>) -> TransportError {
    TransportError {
        code,
        message: message.into(),
    }
}

fn malformed(message: impl Into<String>) -> TransportError {
    error(FailureCode::IntegrityMismatch, message)
}

/// Reads a `200`, `206`, or `416` response that `url` finally returned;
/// any other status is classified by [`status_failure`].
pub(crate) fn read(
    status: StatusCode,
    headers: &HeaderMap,
    url: &Url,
) -> Result<ResponseHead, TransportError> {
    refuse_encoding(headers)?;
    let length = content_length(headers)?;
    let framing = match status {
        StatusCode::OK => Framing::Full { length },
        StatusCode::PARTIAL_CONTENT => partial(headers, length)?,
        StatusCode::RANGE_NOT_SATISFIABLE => Framing::Unsatisfiable {
            complete: match header_str(headers, header::CONTENT_RANGE)? {
                None => None,
                Some(value) => match content_range(value) {
                    Some(ContentRange::Unsatisfied(complete)) => Some(complete),
                    _ => return Err(malformed(format!("invalid Content-Range {value:?}"))),
                },
            },
        },
        other => return Err(status_failure(other, true)),
    };
    let complete = match framing {
        Framing::Full { length } => length,
        Framing::Partial { complete, .. } | Framing::Unsatisfiable { complete } => complete,
    };
    Ok(ResponseHead {
        framing,
        representation: Representation {
            length: complete,
            strong_etag: header_str(headers, header::ETAG)?.and_then(strong_etag),
            last_modified: header_str(headers, header::LAST_MODIFIED)?
                .and_then(|v| httpdate::parse_http_date(v).ok())
                .map(timestamp),
            resource: Some(resource(url)),
        },
        filename: header_str(headers, header::CONTENT_DISPOSITION)?.and_then(disposition_filename),
    })
}

/// Identifies the resource `url` names: a digest of the URL without its
/// fragment, which the client never sends. Validators are compared only
/// within one resource, so any other final URL, even on the same origin,
/// restarts a resume (rule 7.5.5). A digest rather than the URL, which may
/// carry a signed token that must not be stored.
fn resource(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    let digest = Sha256::digest(url.as_str());
    digest.iter().fold(String::from("sha256:"), |mut id, byte| {
        let _ = write!(id, "{byte:02x}");
        id
    })
}

fn timestamp(at: SystemTime) -> UtcTimestamp {
    UtcTimestamp::from_system_time(at)
}

/// Classifies a status that carries no body to download. `replay_safe` is
/// false for a request whose repetition could repeat an action: a server
/// error after it leaves the outcome unknown (T12).
pub(crate) fn status_failure(status: StatusCode, replay_safe: bool) -> TransportError {
    let code = match status.as_u16() {
        500..=599 if !replay_safe => FailureCode::OperationAmbiguous,
        408 | 429 | 500..=599 => FailureCode::RequestTimeout,
        // A signed link that expired answers like a refused one.
        401 | 403 => FailureCode::LeaseExpired,
        // Not found through this link; the resource itself is not known to
        // be gone (failure matrix, 7.4).
        _ => FailureCode::LeaseNotFound,
    };
    error(code, format!("HTTP {status}"))
}

/// Transfers address the stored bytes, so a body in any encoding but
/// `identity` is refused instead of decoded (rule 7.5.6, ADR-15, T07).
fn refuse_encoding(headers: &HeaderMap) -> Result<(), TransportError> {
    for value in headers.get_all(header::CONTENT_ENCODING) {
        let value = value
            .to_str()
            .map_err(|_| malformed("invalid Content-Encoding"))?;
        for coding in value.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            if !coding.eq_ignore_ascii_case("identity") {
                return Err(error(
                    FailureCode::UnsupportedCapability,
                    format!("body is {coding}-encoded; transfers accept identity only"),
                ));
            }
        }
    }
    Ok(())
}

/// One header's value, if present. Several values, or one that is not
/// visible ASCII, are malformed.
fn header_str(
    headers: &HeaderMap,
    name: header::HeaderName,
) -> Result<Option<&str>, TransportError> {
    let mut values = headers.get_all(&name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(malformed(format!("more than one {name}")));
    }
    value
        .to_str()
        .map(|v| Some(v.trim()))
        .map_err(|_| malformed(format!("invalid {name}")))
}

fn content_length(headers: &HeaderMap) -> Result<Option<u64>, TransportError> {
    header_str(headers, header::CONTENT_LENGTH)?
        .map(|v| digits(v).ok_or_else(|| malformed(format!("invalid Content-Length {v:?}"))))
        .transpose()
}

/// A non-negative decimal integer: digits only, so no sign or space slips
/// through `u64::from_str`.
fn digits(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A `206` must carry exactly one `Content-Range`, agree with its
/// `Content-Length`, and never be a multipart answer the engine did not ask
/// for.
fn partial(headers: &HeaderMap, length: Option<u64>) -> Result<Framing, TransportError> {
    if header_str(headers, header::CONTENT_TYPE)?
        .is_some_and(|t| t.to_ascii_lowercase().starts_with("multipart/byteranges"))
    {
        return Err(malformed("multipart range response"));
    }
    let value = header_str(headers, header::CONTENT_RANGE)?
        .ok_or_else(|| malformed("206 without Content-Range"))?;
    let Some(ContentRange::Bytes {
        first,
        last,
        complete,
    }) = content_range(value)
    else {
        return Err(malformed(format!("invalid Content-Range {value:?}")));
    };
    let size = last
        .checked_sub(first)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| malformed(format!("invalid Content-Range {value:?}")))?;
    if length.is_some_and(|length| length != size) {
        return Err(malformed(format!(
            "Content-Length disagrees with Content-Range {value:?}"
        )));
    }
    Ok(Framing::Partial {
        first,
        last,
        complete,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum ContentRange {
    /// `bytes first-last/complete` or `bytes first-last/*`.
    Bytes {
        first: u64,
        last: u64,
        complete: Option<u64>,
    },
    /// `bytes */complete`.
    Unsatisfied(u64),
}

fn content_range(value: &str) -> Option<ContentRange> {
    let (unit, rest) = value.split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (range, complete) = rest.trim_start().split_once('/')?;
    if range == "*" {
        return digits(complete).map(ContentRange::Unsatisfied);
    }
    let (first, last) = range.split_once('-')?;
    Some(ContentRange::Bytes {
        first: digits(first)?,
        last: digits(last)?,
        complete: match complete {
            "*" => None,
            n => Some(digits(n)?),
        },
    })
}

/// A strong entity tag; a weak one (`W/"..."`) cannot make a range
/// conditional and is dropped.
fn strong_etag(value: &str) -> Option<String> {
    let opaque = value.strip_prefix('"')?.strip_suffix('"')?;
    (!opaque.contains('"')).then(|| value.to_owned())
}

/// The file name a `Content-Disposition` suggests (RFC 6266): `filename*`
/// in UTF-8 if present, else `filename`. The engine validates the name.
fn disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    for param in params(value).skip(1) {
        let (name, raw) = param.split_once('=')?;
        let name = name.trim();
        let raw = raw.trim();
        if name.eq_ignore_ascii_case("filename*") {
            let (charset, rest) = raw.split_once('\'')?;
            let (_language, encoded) = rest.split_once('\'')?;
            if charset.eq_ignore_ascii_case("utf-8") {
                return percent_decode_str(encoded)
                    .decode_utf8()
                    .ok()
                    .map(|name| name.into_owned());
            }
        } else if name.eq_ignore_ascii_case("filename") {
            plain = Some(unquote(raw)?);
        }
    }
    plain
}

/// Splits a header value at semicolons outside quoted strings.
fn params(value: &str) -> impl Iterator<Item = &str> {
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let mut cuts = Vec::new();
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                cuts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    cuts.push(&value[start..]);
    cuts.into_iter()
}

/// A token, or a quoted string with its escapes removed.
fn unquote(raw: &str) -> Option<String> {
    let Some(inner) = raw.strip_prefix('"') else {
        return Some(raw.to_owned());
    };
    let inner = inner.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next()? } else { c });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_static(value));
        }
        map
    }

    fn url() -> Url {
        Url::parse("https://cdn.example:8443/a.bin").unwrap()
    }

    #[test]
    fn resources_are_told_apart_by_the_whole_url_without_storing_it() {
        let id = |s: &str| resource(&Url::parse(s).unwrap());
        let base = id("https://cdn.example/a.bin?sig=secret");
        assert!(base.starts_with("sha256:") && base.len() == 71, "{base}");
        assert!(!base.contains("secret"));
        // The fragment is never sent, so it names the same resource.
        assert_eq!(id("https://cdn.example/a.bin?sig=secret#part"), base);
        for other in [
            "https://cdn.example/b.bin?sig=secret",
            "https://cdn.example/a.bin?sig=other",
            "https://cdn.example:8443/a.bin?sig=secret",
            "http://cdn.example/a.bin?sig=secret",
        ] {
            assert_ne!(id(other), base, "{other}");
        }
    }

    #[test]
    fn full_and_partial_responses_are_framed() {
        let head = read(
            StatusCode::OK,
            &headers(&[
                ("content-length", "10"),
                ("etag", "\"v1\""),
                ("last-modified", "Sun, 06 Nov 1994 08:49:37 GMT"),
                ("content-disposition", "attachment; filename=\"a b.zip\""),
            ]),
            &url(),
        )
        .unwrap();
        assert_eq!(head.framing, Framing::Full { length: Some(10) });
        assert_eq!(head.representation.length, Some(10));
        assert_eq!(head.representation.strong_etag.as_deref(), Some("\"v1\""));
        assert_eq!(
            head.representation.last_modified,
            Some(UtcTimestamp::from_unix_millis(784_111_777_000))
        );
        assert_eq!(
            head.representation.resource.as_deref(),
            Some(resource(&url()).as_str())
        );
        assert_eq!(head.filename.as_deref(), Some("a b.zip"));

        let head = read(
            StatusCode::PARTIAL_CONTENT,
            &headers(&[
                ("content-length", "10"),
                ("content-range", "bytes 90-99/100"),
                ("etag", "W/\"weak\""),
            ]),
            &url(),
        )
        .unwrap();
        assert_eq!(
            head.framing,
            Framing::Partial {
                first: 90,
                last: 99,
                complete: Some(100)
            }
        );
        assert_eq!(head.representation.length, Some(100));
        assert_eq!(head.representation.strong_etag, None);

        let head = read(
            StatusCode::RANGE_NOT_SATISFIABLE,
            &headers(&[("content-range", "bytes */100")]),
            &url(),
        )
        .unwrap();
        assert_eq!(
            head.framing,
            Framing::Unsatisfiable {
                complete: Some(100)
            }
        );
    }

    #[test]
    fn malformed_framing_is_refused() {
        let cases: &[(StatusCode, &[(&'static str, &'static str)])] = &[
            (StatusCode::OK, &[("content-length", "+10")]),
            (StatusCode::OK, &[("content-length", "ten")]),
            (StatusCode::PARTIAL_CONTENT, &[("content-length", "10")]),
            (
                StatusCode::PARTIAL_CONTENT,
                &[("content-range", "bytes 9-0/100")],
            ),
            (
                StatusCode::PARTIAL_CONTENT,
                &[("content-range", "bytes 0-9/100"), ("content-length", "11")],
            ),
            (
                StatusCode::PARTIAL_CONTENT,
                &[("content-range", "items 0-9/100")],
            ),
            (
                StatusCode::PARTIAL_CONTENT,
                &[
                    ("content-range", "bytes 0-9/100"),
                    ("content-range", "bytes 10-19/100"),
                ],
            ),
            (
                StatusCode::PARTIAL_CONTENT,
                &[
                    ("content-range", "bytes 0-9/100"),
                    ("content-type", "multipart/byteranges; boundary=x"),
                ],
            ),
            (
                StatusCode::RANGE_NOT_SATISFIABLE,
                &[("content-range", "bytes 0-9/100")],
            ),
        ];
        for (status, pairs) in cases {
            let err = read(*status, &headers(pairs), &url()).unwrap_err();
            assert_eq!(err.code, FailureCode::IntegrityMismatch, "{pairs:?}");
        }
    }

    #[test]
    fn encoded_bodies_are_refused_not_decoded() {
        for coding in ["gzip", "br", "identity, gzip"] {
            let mut map = headers(&[("content-length", "10")]);
            map.insert("content-encoding", HeaderValue::from_str(coding).unwrap());
            let err = read(StatusCode::OK, &map, &url()).unwrap_err();
            assert_eq!(err.code, FailureCode::UnsupportedCapability, "{coding}");
        }
        let identity = headers(&[("content-encoding", "identity")]);
        assert!(read(StatusCode::OK, &identity, &url()).is_ok());
    }

    #[test]
    fn statuses_are_classified_by_what_they_mean_for_a_lease() {
        let code =
            |status: u16, safe| status_failure(StatusCode::from_u16(status).unwrap(), safe).code;
        assert_eq!(code(404, true), FailureCode::LeaseNotFound);
        assert_eq!(code(410, true), FailureCode::LeaseNotFound);
        assert_eq!(code(403, true), FailureCode::LeaseExpired);
        assert_eq!(code(503, true), FailureCode::RequestTimeout);
        assert_eq!(code(429, true), FailureCode::RequestTimeout);
        // A server error after a consumptive request leaves its outcome
        // unknown.
        assert_eq!(code(500, false), FailureCode::OperationAmbiguous);
        assert_eq!(code(404, false), FailureCode::LeaseNotFound);
    }

    #[test]
    fn disposition_names_prefer_the_utf8_form() {
        let name = |v| disposition_filename(v);
        assert_eq!(
            name("attachment; filename=\"plain.zip\"; filename*=UTF-8''%E2%82%AC%20rates.zip")
                .as_deref(),
            Some("€ rates.zip")
        );
        assert_eq!(
            name("attachment; filename=token.bin").as_deref(),
            Some("token.bin")
        );
        assert_eq!(
            name(r#"attachment; filename="semi;colon \"q\".txt""#).as_deref(),
            Some("semi;colon \"q\".txt")
        );
        assert_eq!(name("inline").as_deref(), None);
        assert_eq!(name("attachment; filename=\"unterminated").as_deref(), None);
    }
}
