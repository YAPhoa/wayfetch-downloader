//! Secret bytes that never print, serialize, or outlive their use in
//! memory longer than needed (architecture brief section 8.4).

use zeroize::Zeroizing;

/// Bytes that must not leak: credentials, tokens, device codes, and
/// provider responses that carry them. Wiped when dropped. Its `Debug`
/// form is redacted and it has no `Display` or serde implementation, so it
/// cannot reach a log, an error message, or a client by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    /// Takes ownership of `bytes`.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Copies `text`.
    pub fn from_text(text: &str) -> Self {
        Self::new(text.as_bytes().to_vec())
    }

    /// The bytes, for the one place that needs them.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The bytes as UTF-8 text, if they are.
    pub fn expose_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether it holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_secret() {
        let secret = Secret::from_text("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret([redacted])");
        assert_eq!(
            format!("{:?}", Some(secret.clone())),
            "Some(Secret([redacted]))"
        );
        assert_eq!(secret.expose_text(), Some("hunter2"));
    }
}
