//! Bearer-token authentication with scoped permissions (architecture brief
//! sections 4.6 and 8.5).
//!
//! The host authenticates; the engine authorizes. A request's bearer token
//! is looked up here and the permissions it was issued with are handed to
//! the engine, which checks each command, query, and subscription against
//! them. No request payload can name its own permissions.
//!
//! Tokens come from an operator-owned file, one per line:
//!
//! ```text
//! # comment
//! <token> read,submit,control
//! <token> read
//! ```
//!
//! Only SHA-256 digests of the tokens are kept in memory and compared. On
//! Unix, a file other users can read or write is refused.
//!
//! When browser access is configured, a request without an
//! `Authorization` header may instead present an owner session cookie;
//! see [`crate::browser`].

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use wayfetch_engine::protocol::Permission;

use crate::error::ApiError;
use crate::routes::AppState;
use crate::secret::{self, ReadError};

/// Largest token file accepted.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// Shortest accepted token: 128 bits of hex.
const MIN_TOKEN_CHARS: usize = 32;
/// Longest accepted token.
const MAX_TOKEN_CHARS: usize = 256;

/// The permissions an authenticated request was granted.
#[derive(Debug, Clone)]
pub struct Grant(pub Arc<[Permission]>);

/// A token file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum TokenFileError {
    /// The file could not be read or created.
    #[error("token file {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Other users may read or change the file.
    #[error("token file {0} is accessible to other users; make it owner-only (chmod 600)")]
    Exposed(PathBuf),
    /// The file is larger than any sensible token list.
    #[error("token file {0} is larger than {MAX_FILE_BYTES} bytes")]
    TooLarge(PathBuf),
    /// A line is not `<token> <permission>[,<permission>...]`. The line's
    /// content is not repeated, since it may hold a token.
    #[error("token file {path}, line {line}: {reason}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// What is wrong.
        reason: &'static str,
    },
    /// The file holds no token, so nobody could use the API.
    #[error("token file {0} holds no token")]
    Empty(PathBuf),
}

/// The tokens the API accepts.
#[derive(Debug, Clone, Default)]
pub struct Tokens {
    by_digest: HashMap<[u8; 32], Arc<[Permission]>>,
}

impl Tokens {
    /// Reads the token file at `path`.
    pub fn load(path: &Path) -> Result<Self, TokenFileError> {
        let text = secret::read_private(path, MAX_FILE_BYTES).map_err(|e| match e {
            ReadError::Io(source) => TokenFileError::Io {
                path: path.to_owned(),
                source,
            },
            ReadError::Exposed => TokenFileError::Exposed(path.to_owned()),
            ReadError::TooLarge => TokenFileError::TooLarge(path.to_owned()),
        })?;
        Self::parse(&text).map_err(|e| match e {
            ParseError::Line(line, reason) => TokenFileError::Invalid {
                path: path.to_owned(),
                line,
                reason,
            },
            ParseError::Empty => TokenFileError::Empty(path.to_owned()),
        })
    }

    /// Reads the token file at `path`, first creating it, owner-only, with
    /// one fresh token holding every permission if it does not exist.
    /// Returns whether it was created. The token is never printed; the
    /// operator reads it from the file.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool), TokenFileError> {
        let created = match secret::create_private(path) {
            Ok(mut file) => {
                let token = secret::generate().map_err(|source| TokenFileError::Io {
                    path: path.to_owned(),
                    source,
                })?;
                let all = Permission::ALL
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",");
                let text = format!(
                    "# wayfetchd API tokens: one per line, \"<token> <permission>[,...]\".\n\
                     # Permissions: read, submit, control, sources (submitted links),\n\
                     # accounts (provider sign-ins), provider_downloads (downloading\n\
                     # through accounts); neither is implied by another.\n\
                     {token} {all}\n"
                );
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|source| TokenFileError::Io {
                        path: path.to_owned(),
                        source,
                    })?;
                true
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => false,
            Err(source) => {
                return Err(TokenFileError::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Ok((Self::load(path)?, created))
    }

    fn parse(text: &str) -> Result<Self, ParseError> {
        let mut by_digest = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let invalid = |reason| ParseError::Line(index + 1, reason);
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut fields = line.split_ascii_whitespace();
            let (Some(token), Some(scopes), None) = (fields.next(), fields.next(), fields.next())
            else {
                return Err(invalid("expected `<token> <permission>[,...]`"));
            };
            if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&token.len()) {
                return Err(invalid("token must be 32 to 256 characters"));
            }
            let mut granted = Vec::new();
            for scope in scopes.split(',') {
                let permission = Permission::from_str(scope)
                    .map_err(|_| invalid("unknown permission; use read, submit, or control"))?;
                if !granted.contains(&permission) {
                    granted.push(permission);
                }
            }
            if by_digest
                .insert(secret::digest(token), Arc::from(granted))
                .is_some()
            {
                return Err(invalid("token listed twice"));
            }
        }
        if by_digest.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(Self { by_digest })
    }

    /// The permissions of `token`, if it is known.
    fn grant(&self, token: &str) -> Option<Grant> {
        self.by_digest
            .get(&secret::digest(token))
            .cloned()
            .map(Grant)
    }
}

#[derive(Debug)]
enum ParseError {
    Line(usize, &'static str),
    Empty,
}

/// Middleware: admits a request with a known bearer token or, when
/// browser access is configured, an owner session, and attaches its
/// [`Grant`], and for a session its [`SessionEnd`](crate::browser::SessionEnd). A request that sends an `Authorization` header is judged by
/// that header alone.
pub async fn authenticate(
    State(app): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let grant = match (request.headers().get(header::AUTHORIZATION), &app.browser) {
        (Some(value), _) => value
            .to_str()
            .ok()
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .and_then(|(_, token)| app.tokens.grant(token.trim()))
            .ok_or_else(ApiError::unauthenticated)?,
        (None, Some(browser)) => {
            let (grant, end) = browser.authenticate(request.method(), request.headers())?;
            request.extensions_mut().insert(end);
            grant
        }
        (None, None) => return Err(ApiError::unauthenticated()),
    };
    request.extensions_mut().insert(grant);
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0123456789abcdef0123456789abcdef";
    const B: &str = "fedcba9876543210fedcba9876543210";

    fn line_error(text: &str) -> usize {
        match Tokens::parse(text) {
            Err(ParseError::Line(line, _)) => line,
            _ => panic!("{text:?} parsed"),
        }
    }

    #[test]
    fn tokens_carry_their_own_permissions() {
        let tokens = Tokens::parse(&format!(
            "# owner\n\n{A} read,submit,control\n  {B}   read,read  \n"
        ))
        .unwrap();
        assert_eq!(
            &*tokens.grant(A).unwrap().0,
            [Permission::Read, Permission::Submit, Permission::Control]
        );
        assert_eq!(&*tokens.grant(B).unwrap().0, [Permission::Read]);
        assert!(tokens.grant(&A[1..]).is_none());
        assert!(tokens.grant("").is_none());
    }

    #[test]
    fn malformed_lines_are_refused_by_number() {
        assert_eq!(line_error(&format!("{A}\n")), 1);
        assert_eq!(line_error(&format!("#\n{A} read extra\n")), 2);
        assert_eq!(line_error("short read\n"), 1);
        assert_eq!(line_error(&format!("{A} admin\n")), 1);
        assert_eq!(line_error(&format!("{A} read\n{A} control\n")), 2);
        assert!(matches!(Tokens::parse("# none\n"), Err(ParseError::Empty)));
    }

    #[test]
    fn a_created_file_is_owner_only_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tokens");
        let (created, fresh) = Tokens::load_or_create(&path).unwrap();
        assert!(fresh);
        let text = std::fs::read_to_string(&path).unwrap();
        let line = text.lines().find(|l| !l.starts_with('#')).unwrap();
        let (token, scopes) = line.split_once(' ').unwrap();
        assert_eq!(token.len(), 2 * secret::SECRET_BYTES);
        assert_eq!(
            scopes,
            "read,submit,control,sources,accounts,provider_downloads"
        );
        assert_eq!(created.grant(token).unwrap().0.len(), 6);

        let (again, fresh) = Tokens::load_or_create(&path).unwrap();
        assert!(!fresh);
        assert!(again.grant(token).is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tokens");
        std::fs::write(&path, format!("{A} read\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Tokens::load(&path),
            Err(TokenFileError::Exposed(_))
        ));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Tokens::load(&path).is_ok());
    }
}
