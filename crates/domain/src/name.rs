//! Validated, bounded names used in identities and diagnostics.

use std::fmt;

/// A value failed validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {reason}")]
pub struct InvalidName {
    kind: &'static str,
    reason: &'static str,
}

impl InvalidName {
    pub(crate) const fn new(kind: &'static str, reason: &'static str) -> Self {
        Self { kind, reason }
    }
}

/// Defines a `String` newtype whose every constructor runs `$validate`, which
/// returns the (possibly normalized) text or an error. The newtype converts
/// to and from `String` and `&str`, and serializes as its text.
macro_rules! validated_string {
    ($(#[$meta:meta])* $name:ident, $validate:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(
            feature = "serde",
            derive(serde::Serialize, serde::Deserialize),
            serde(try_from = "String", into = "String")
        )]
        #[cfg_attr(feature = "ts", derive(ts_rs::TS))]
        pub struct $name(String);

        impl $name {
            /// Validates the text.
            pub fn new(value: impl Into<String>) -> Result<Self, InvalidName> {
                let validate: fn(String) -> Result<String, InvalidName> = $validate;
                validate(value.into()).map(Self)
            }

            /// The validated text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::str::FromStr for $name {
            type Err = InvalidName;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidName;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

/// Maximum length of a [`Namespace`], [`ProviderId`], or [`PluginId`] in
/// bytes.
pub const SLUG_MAX_LEN: usize = 64;

fn slug(kind: &'static str) -> impl Fn(String) -> Result<String, InvalidName> {
    move |value| {
        let valid_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
        match value.as_bytes() {
            [] => Err(InvalidName::new(kind, "must not be empty")),
            v if v.len() > SLUG_MAX_LEN => Err(InvalidName::new(kind, "must be at most 64 bytes")),
            [first, ..] if !valid_char(*first) => {
                Err(InvalidName::new(kind, "must start with a-z or 0-9"))
            }
            v if !v
                .iter()
                .all(|&b| valid_char(b) || matches!(b, b'.' | b'_' | b'-')) =>
            {
                Err(InvalidName::new(
                    kind,
                    "may contain only a-z, 0-9, '.', '_' and '-'",
                ))
            }
            _ => Ok(value),
        }
    }
}

fn bounded_text(
    kind: &'static str,
    max_len: usize,
) -> impl Fn(String) -> Result<String, InvalidName> {
    move |value| {
        if value.is_empty() {
            Err(InvalidName::new(kind, "must not be empty"))
        } else if value.len() > max_len {
            Err(InvalidName::new(kind, "too long"))
        } else if value.chars().any(char::is_control) {
            Err(InvalidName::new(
                kind,
                "must not contain control characters",
            ))
        } else {
            Ok(value)
        }
    }
}

validated_string!(
    /// An adapter namespace such as `direct-http`: lowercase ASCII letters,
    /// digits, `.`, `_` and `-`, starting with a letter or digit, 1 to 64
    /// bytes.
    Namespace,
    |v| slug("namespace")(v)
);

validated_string!(
    /// A resolver or provider identity, distinct from the resource's original
    /// host (architecture brief section 3.4). Same syntax as [`Namespace`].
    ProviderId,
    |v| slug("provider id")(v)
);

validated_string!(
    /// A helper program's identity, such as `yt-dlp`, stable across its
    /// versions. Same syntax as [`Namespace`].
    PluginId,
    |v| slug("plugin id")(v)
);

/// Maximum length of a [`CanonicalKey`] in bytes.
pub const CANONICAL_KEY_MAX_LEN: usize = 1024;

validated_string!(
    /// An adapter-defined canonical key for a resource within its namespace.
    /// Compared byte for byte; the adapter owns its normalization. Not a URL
    /// to fetch: a signed URL is a request, not an identity (section 6.1).
    CanonicalKey,
    |v| bounded_text("canonical key", CANONICAL_KEY_MAX_LEN)(v)
);

/// Maximum length of a [`Variant`] in bytes.
pub const VARIANT_MAX_LEN: usize = 128;

validated_string!(
    /// A variant of a resource, such as a quality or format selection.
    Variant,
    |v| bounded_text("variant", VARIANT_MAX_LEN)(v)
);

validated_string!(
    /// A normalized host as defined by the WHATWG URL Standard and parsed by
    /// the `url` crate: a lowercase ASCII domain (internationalized names
    /// become punycode), an IPv4 address, or a bracketed IPv6 address. No
    /// scheme, port, or path, and at most 253 bytes with no empty labels.
    HostName,
    |v| {
        const KIND: &str = "host name";
        let host = url::Host::parse(&v).map_err(|_| InvalidName::new(KIND, "not a valid host"))?;
        if let url::Host::Domain(domain) = &host {
            let domain = domain.strip_suffix('.').unwrap_or(domain);
            if domain.len() > 253 || domain.split('.').any(|l| l.is_empty() || l.len() > 63) {
                return Err(InvalidName::new(KIND, "labels must be 1 to 63 bytes, 253 in total"));
            }
            return Ok(domain.to_owned());
        }
        Ok(host.to_string())
    }
);

/// Maximum length of [`DiagnosticText`] in bytes.
pub const DIAGNOSTIC_MAX_LEN: usize = 512;

/// Bounded diagnostic text. Callers must redact secrets before constructing
/// it; this type only bounds size and removes control characters so it is
/// safe to store and display.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(from = "String", into = "String")
)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
pub struct DiagnosticText(String);

impl DiagnosticText {
    /// Builds diagnostic text, replacing control characters with spaces and
    /// truncating at a character boundary to [`DIAGNOSTIC_MAX_LEN`] bytes.
    pub fn new(value: &str) -> Self {
        let mut out = String::with_capacity(value.len().min(DIAGNOSTIC_MAX_LEN));
        for c in value.chars().map(|c| if c.is_control() { ' ' } else { c }) {
            if out.len() + c.len_utf8() > DIAGNOSTIC_MAX_LEN {
                break;
            }
            out.push(c);
        }
        Self(out)
    }

    /// The bounded text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for DiagnosticText {
    fn from(value: String) -> Self {
        Self::new(&value)
    }
}

impl From<DiagnosticText> for String {
    fn from(value: DiagnosticText) -> Self {
        value.0
    }
}

impl fmt::Display for DiagnosticText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_syntax() {
        assert!(Namespace::new("direct-http").is_ok());
        assert!(Namespace::new("a").is_ok());
        for bad in ["", "Direct", "-x", ".x", "a b", "a/b", &"a".repeat(65)] {
            assert!(Namespace::new(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn host_names_are_normalized() {
        for (input, expected) in [
            ("Example.COM.", "example.com"),
            ("bücher.example", "xn--bcher-kva.example"),
            ("192.0.2.1", "192.0.2.1"),
            ("[2001:DB8::1]", "[2001:db8::1]"),
        ] {
            assert_eq!(HostName::new(input).unwrap().as_str(), expected, "{input}");
        }
    }

    #[test]
    fn host_names_reject_urls_and_garbage() {
        for bad in [
            "",
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "ex ample.com",
            "a..example",
            "1.2.3.999",
            "[not-ipv6]",
            "2001:db8::1",
            &format!("{}.example", "a".repeat(64)),
        ] {
            assert!(HostName::new(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn canonical_key_bounds() {
        assert!(CanonicalKey::new("file/abc123").is_ok());
        assert!(CanonicalKey::new("").is_err());
        assert!(CanonicalKey::new("a\nb").is_err());
        assert!(CanonicalKey::new("a".repeat(CANONICAL_KEY_MAX_LEN + 1)).is_err());
    }

    #[test]
    fn diagnostic_text_is_bounded_and_printable() {
        let text = DiagnosticText::new("line1\nline2\u{7}");
        assert_eq!(text.as_str(), "line1 line2 ");
        let text = DiagnosticText::new(&"é".repeat(DIAGNOSTIC_MAX_LEN));
        assert!(text.as_str().len() <= DIAGNOSTIC_MAX_LEN);
        assert!(text.as_str().chars().all(|c| c == 'é'));
    }
}
