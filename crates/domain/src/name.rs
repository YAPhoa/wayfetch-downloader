//! Validated, bounded names used in identities and diagnostics.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// A value failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidName {
    kind: &'static str,
    reason: &'static str,
}

impl InvalidName {
    fn new(kind: &'static str, reason: &'static str) -> Self {
        Self { kind, reason }
    }

    pub(crate) fn provider_code() -> Self {
        Self::new("provider code", "must be 1 to 64 printable ASCII bytes")
    }
}

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for InvalidName {}

macro_rules! string_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
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
    };
}

fn validate_slug(kind: &'static str, value: &str) -> Result<(), InvalidName> {
    if value.is_empty() || value.len() > Namespace::MAX_LEN {
        return Err(InvalidName::new(kind, "must be 1 to 64 bytes"));
    }
    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(InvalidName::new(kind, "must start with a-z or 0-9"));
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(InvalidName::new(
            kind,
            "may contain only a-z, 0-9, '.', '_' and '-'",
        ));
    }
    Ok(())
}

string_newtype!(
    /// An adapter namespace such as `direct-http`. Lowercase ASCII letters,
    /// digits, `.`, `_` and `-`; starts with a letter or digit; 1 to 64 bytes.
    Namespace
);

impl Namespace {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 64;

    /// Validates a namespace.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidName> {
        let value = value.into();
        validate_slug("namespace", &value)?;
        Ok(Self(value))
    }
}

string_newtype!(
    /// A resolver or provider identity, distinct from the resource's original
    /// host (architecture brief section 3.4). Same syntax as [`Namespace`].
    ProviderId
);

impl ProviderId {
    /// Validates a provider identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidName> {
        let value = value.into();
        validate_slug("provider id", &value)?;
        Ok(Self(value))
    }
}

string_newtype!(
    /// A normalized host: a lowercase ASCII DNS name (IDNs in punycode form),
    /// an IPv4 address, or a bracketed IPv6 address. No scheme, port, path,
    /// or trailing dot.
    HostName
);

impl HostName {
    /// Maximum DNS name length in bytes.
    pub const MAX_LEN: usize = 253;

    /// Validates and normalizes a host. ASCII letters are lowercased and one
    /// trailing dot is removed; nothing else is rewritten.
    pub fn new(value: &str) -> Result<Self, InvalidName> {
        const KIND: &str = "host name";
        if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            let addr: Ipv6Addr = inner
                .parse()
                .map_err(|_| InvalidName::new(KIND, "invalid IPv6 literal"))?;
            return Ok(Self(format!("[{addr}]")));
        }
        let value = value
            .strip_suffix('.')
            .unwrap_or(value)
            .to_ascii_lowercase();
        if value.is_empty() || value.len() > Self::MAX_LEN {
            return Err(InvalidName::new(KIND, "must be 1 to 253 bytes"));
        }
        if value.parse::<Ipv4Addr>().is_ok() {
            return Ok(Self(value));
        }
        for label in value.split('.') {
            if label.is_empty() || label.len() > 63 {
                return Err(InvalidName::new(KIND, "each label must be 1 to 63 bytes"));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(InvalidName::new(
                    KIND,
                    "labels may not start or end with '-'",
                ));
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                return Err(InvalidName::new(
                    KIND,
                    "labels may contain only a-z, 0-9 and '-'",
                ));
            }
        }
        // A dotted all-numeric name that is not a valid IPv4 address (such as
        // `1.2.3.999`) is ambiguous between resolvers; reject it.
        let last = value.rsplit('.').next().unwrap_or_default();
        if last.bytes().all(|b| b.is_ascii_digit()) {
            return Err(InvalidName::new(KIND, "numeric top-level label"));
        }
        Ok(Self(value))
    }
}

fn validate_text(kind: &'static str, value: &str, max_len: usize) -> Result<(), InvalidName> {
    if value.is_empty() {
        return Err(InvalidName::new(kind, "must not be empty"));
    }
    if value.len() > max_len {
        return Err(InvalidName::new(kind, "too long"));
    }
    if value.chars().any(char::is_control) {
        return Err(InvalidName::new(
            kind,
            "must not contain control characters",
        ));
    }
    Ok(())
}

string_newtype!(
    /// An adapter-defined canonical key for a resource within its namespace.
    /// Compared byte for byte; the adapter owns its normalization. Not a URL
    /// to fetch: a signed URL is a request, not an identity (section 6.1).
    CanonicalKey
);

impl CanonicalKey {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 1024;

    /// Validates a canonical key.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidName> {
        let value = value.into();
        validate_text("canonical key", &value, Self::MAX_LEN)?;
        Ok(Self(value))
    }
}

string_newtype!(
    /// A variant of a resource, such as a quality or format selection.
    Variant
);

impl Variant {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 128;

    /// Validates a variant label.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidName> {
        let value = value.into();
        validate_text("variant", &value, Self::MAX_LEN)?;
        Ok(Self(value))
    }
}

string_newtype!(
    /// Bounded diagnostic text. Callers must redact secrets before
    /// constructing it; this type only bounds size and removes control
    /// characters so it is safe to store and display.
    DiagnosticText
);

impl DiagnosticText {
    /// Maximum length in bytes.
    pub const MAX_LEN: usize = 512;

    /// Builds diagnostic text, replacing control characters with spaces and
    /// truncating at a character boundary to [`Self::MAX_LEN`] bytes.
    pub fn new(value: &str) -> Self {
        let mut out = String::with_capacity(value.len().min(Self::MAX_LEN));
        for c in value.chars() {
            let c = if c.is_control() { ' ' } else { c };
            if out.len() + c.len_utf8() > Self::MAX_LEN {
                break;
            }
            out.push(c);
        }
        Self(out)
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
        assert_eq!(
            HostName::new("Example.COM.").unwrap().as_str(),
            "example.com"
        );
        assert_eq!(
            HostName::new("xn--bcher-kva.example").unwrap().as_str(),
            "xn--bcher-kva.example"
        );
        assert_eq!(HostName::new("192.0.2.1").unwrap().as_str(), "192.0.2.1");
        assert_eq!(
            HostName::new("[2001:DB8::1]").unwrap().as_str(),
            "[2001:db8::1]"
        );
    }

    #[test]
    fn host_names_reject_urls_and_garbage() {
        for bad in [
            "",
            ".",
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "ex ample.com",
            "-a.example",
            "a..example",
            "bücher.example",
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
        assert!(CanonicalKey::new("a".repeat(CanonicalKey::MAX_LEN + 1)).is_err());
    }

    #[test]
    fn diagnostic_text_is_bounded_and_printable() {
        let text = DiagnosticText::new("line1\nline2\u{7}");
        assert_eq!(text.as_str(), "line1 line2 ");
        let long = "é".repeat(DiagnosticText::MAX_LEN);
        let text = DiagnosticText::new(&long);
        assert!(text.as_str().len() <= DiagnosticText::MAX_LEN);
        assert!(text.as_str().chars().all(|c| c == 'é'));
    }
}
