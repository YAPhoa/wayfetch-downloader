//! Browser access: the local owner account, sign-in, sessions, and
//! protection from other sites (architecture brief sections 8.5 and 8.6).
//!
//! A server installation creates its own owner account: at startup, while
//! none exists, a single-use setup secret valid for 15 minutes is written
//! to an owner-only file in the operator's auth directory, and whoever can
//! read that file completes setup in the browser with it, choosing the
//! owner's name and password. Afterwards the owner signs in with them.
//!
//! A signed-in browser holds an opaque session cookie (`HttpOnly`,
//! `SameSite=Strict`, `Secure`, host-only) and is granted every
//! permission, like the operator's own token. Requests made with the
//! cookie must address and come from a configured origin, and every
//! state-changing one must repeat the session's CSRF token in the
//! `X-CSRF-Token` header. Cookies without `Secure` exist only in the
//! loopback development profile, where every origin is plain-HTTP
//! loopback and the listener itself is loopback-only.
//!
//! Sign-in and setup attempts share one rate limit, and password hashing
//! runs one at a time, so guessing is slow and cannot exhaust memory or
//! CPU. Failures say only that authentication failed.
//!
//! Bearer tokens are unaffected: a request with an `Authorization` header
//! is judged by its token alone, whatever cookies it carries.

mod origin;
mod owner;
mod session;

pub(crate) mod routes;

pub(crate) use self::session::SessionEnd;

use std::fs::{File, TryLockError};
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, Method, header};
use cookie::{Cookie, SameSite};
use governor::clock::Clock;
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use tokio::sync::Semaphore;
use url::Url;
use wayfetch_engine::protocol::{ErrorCode, Permission};

use self::origin::OriginPolicy;
use self::owner::{OWNER_FILE, Owner, SETUP_FILE, SetupSecret};
use self::session::{Issued, Sessions};
use crate::auth::Grant;
use crate::error::ApiError;
use crate::secret;

/// What a signed-in owner's browser may do: everything the owner may.
pub(crate) const OWNER: &[Permission] = Permission::ALL;

/// The lock file that makes one process the auth directory's user.
const LOCK_FILE: &str = "auth.lock";
/// The session cookie over HTTPS. The `__Host-` prefix makes browsers
/// insist on `Secure`, `Path=/`, and no `Domain`, so no other host, even
/// a sibling subdomain, can set or shadow it.
const SECURE_COOKIE: &str = "__Host-wayfetch-session";
/// The session cookie in the loopback development profile.
const LOOPBACK_COOKIE: &str = "wayfetch-session";
/// The header that carries the CSRF token.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// Sign-in and setup attempts allowed per minute, all clients together;
/// also the largest burst.
const ATTEMPTS_PER_MINUTE: NonZeroU32 = NonZeroU32::new(5).unwrap();

/// How browsers reach the server.
#[derive(Debug, Clone)]
pub struct BrowserConfig {
    /// Owner-only directory for the owner account, the setup secret, and
    /// a lock. Created owner-only if missing; its parent must exist.
    pub auth_dir: PathBuf,
    /// The origins browsers use, such as `https://wayfetch.example.lan`:
    /// the reverse proxy's public address, not the listener's.
    pub origins: Vec<Url>,
    /// Loopback development: plain-HTTP loopback origins and cookies
    /// without `Secure`. Allowed only on a loopback listener; without
    /// origins, the listener's own address and `localhost` are used.
    pub insecure_loopback: bool,
}

/// A setup secret was written.
#[derive(Debug, Clone)]
pub struct SetupIssued {
    /// The file holding it.
    pub path: PathBuf,
    /// How long it is accepted.
    pub valid_for: Duration,
}

/// Browser access could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum BrowserAuthError {
    /// A file or directory could not be used.
    #[error("{path}: {source}")]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Other users may read or change it.
    #[error(
        "{0} is accessible to other users; make it owner-only (chmod 700 for the directory, 600 for files)"
    )]
    Exposed(PathBuf),
    /// The owner account file is damaged.
    #[error("owner account {0}: {1}")]
    InvalidOwner(PathBuf, &'static str),
    /// Another process uses the auth directory.
    #[error("auth directory {0} is in use by another wayfetchd; stop it first")]
    Locked(PathBuf),
    /// Browser access needs at least one origin.
    #[error("browser access needs at least one browser origin")]
    NoOrigin,
    /// An origin cannot be used.
    #[error("browser origin {origin}: {reason}")]
    BadOrigin {
        /// The origin as given.
        origin: String,
        /// Why not.
        reason: &'static str,
    },
    /// Loopback development was asked for on a listener beyond loopback.
    #[error("loopback development needs a loopback listener, not {0}")]
    NotLoopback(SocketAddr),
}

/// The browser access of one server.
#[derive(Debug)]
pub struct BrowserAuth {
    dir: PathBuf,
    /// Held while this exists.
    _lock: File,
    origins: OriginPolicy,
    secure_cookies: bool,
    owner: RwLock<Option<Arc<Owner>>>,
    /// The setup secret in force, if setup is pending. Held through a
    /// whole setup, so setups run one at a time.
    setup: tokio::sync::Mutex<Option<SetupSecret>>,
    sessions: Mutex<Sessions>,
    attempts: DefaultDirectRateLimiter,
    /// One permit: password hashing runs one at a time.
    hashing: Arc<Semaphore>,
}

impl BrowserAuth {
    /// Browser access for a server listening on `listen`. Takes the auth
    /// directory's lock, loads the owner account, and, if there is none,
    /// writes a fresh setup secret, returned as [`SetupIssued`]. If there
    /// is an owner, a leftover setup secret is removed.
    pub fn open(
        config: &BrowserConfig,
        listen: SocketAddr,
    ) -> Result<(Self, Option<SetupIssued>), BrowserAuthError> {
        let origins = if config.insecure_loopback {
            if !listen.ip().is_loopback() {
                return Err(BrowserAuthError::NotLoopback(listen));
            }
            if config.origins.is_empty() {
                loopback_origins(listen)
            } else {
                config.origins.clone()
            }
        } else {
            config.origins.clone()
        };
        let origins = OriginPolicy::new(&origins, config.insecure_loopback)?;
        let dir = &config.auth_dir;
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| BrowserAuthError::Io { path, source }
        };
        if !secret::private_dir(dir).map_err(io_err(dir))? {
            return Err(BrowserAuthError::Exposed(dir.clone()));
        }
        let lock = lock(dir)?;
        let owner = Owner::load(dir)?;
        let setup_path = dir.join(SETUP_FILE);
        let (setup, issued) = match owner {
            Some(_) => {
                secret::remove(&setup_path).map_err(io_err(&setup_path))?;
                (None, None)
            }
            None => {
                let setup = SetupSecret::issue(dir, Instant::now()).map_err(io_err(&setup_path))?;
                let issued = SetupIssued {
                    path: setup_path,
                    valid_for: owner::SETUP_LIFETIME,
                };
                (Some(setup), Some(issued))
            }
        };
        let auth = Self {
            dir: dir.clone(),
            _lock: lock,
            origins,
            secure_cookies: !config.insecure_loopback,
            owner: RwLock::new(owner.map(Arc::new)),
            setup: tokio::sync::Mutex::new(setup),
            sessions: Mutex::default(),
            attempts: RateLimiter::direct(Quota::per_minute(ATTEMPTS_PER_MINUTE)),
            hashing: Arc::new(Semaphore::new(1)),
        };
        Ok((auth, issued))
    }

    /// The origins browsers may use, as they send them.
    pub fn origins(&self) -> &[String] {
        self.origins.origins()
    }

    /// The grant of a browser request's session, and when what the request
    /// opens must end with the session: refused unless the request has a
    /// live session, addresses and comes from an allowed origin, and, if it
    /// changes state, repeats the session's CSRF token.
    pub(crate) fn authenticate(
        &self,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<(Grant, SessionEnd), ApiError> {
        let tokens = self.cookie_tokens(headers);
        if tokens.is_empty() {
            return Err(ApiError::unauthenticated());
        }
        self.origins.check(method, headers)?;
        let live = self
            .live_session(&tokens)
            .ok_or_else(ApiError::unauthenticated)?;
        if !method.is_safe() {
            check_csrf(headers, &live.csrf)?;
        }
        Ok((Grant(Arc::from(OWNER)), live.end))
    }

    /// Whether setup is still to be done.
    fn setup_required(&self) -> bool {
        self.owner().is_none()
    }

    fn owner(&self) -> Option<Arc<Owner>> {
        self.owner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Admits one sign-in or setup attempt, or says when to try again.
    fn attempt(&self) -> Result<(), ApiError> {
        self.attempts.check().map_err(|not_until| {
            ApiError::rate_limited(not_until.wait_time_from(self.attempts.clock().now()))
        })
    }

    /// Creates the owner account with the setup secret, and signs its
    /// browser in.
    async fn complete_setup(
        &self,
        setup_secret: &str,
        username: String,
        password: String,
    ) -> Result<(Issued, String), ApiError> {
        self.attempt()?;
        let refused = || {
            ApiError::unauthenticated_because(
                "setup failed: the setup secret is invalid or expired",
            )
        };
        let mut setup = self.setup.lock().await;
        if !self.setup_required() {
            return Err(already_set_up());
        }
        match &*setup {
            Some(secret) if secret.expired(Instant::now()) => {
                *setup = None;
                let _ = secret::remove(&self.dir.join(SETUP_FILE));
                return Err(refused());
            }
            Some(secret) if secret.matches(setup_secret) => {}
            _ => return Err(refused()),
        }
        owner::validate(&username, &password)?;
        let dir = self.dir.clone();
        let created = self
            .hash(move || {
                let owner = Owner::new(username, &password)?;
                Ok(owner.publish(&dir)?.then_some(owner))
            })
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal, "the owner account was not saved"))?
            .ok_or_else(already_set_up)?;
        // Used once: the secret is gone whatever happens to its file.
        *setup = None;
        let _ = secret::remove(&self.dir.join(SETUP_FILE));
        let username = created.username.clone();
        *self.owner.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(created));
        Ok((self.start_session()?, username))
    }

    /// Signs a browser in as the owner.
    async fn login(&self, username: String, password: String) -> Result<Issued, ApiError> {
        self.attempt()?;
        let failed = || ApiError::unauthenticated_because("authentication failed");
        let owner = self.owner().ok_or_else(failed)?;
        if password.len() > owner::MAX_PASSWORD_BYTES {
            return Err(failed());
        }
        let verified = self
            .hash(move || Ok(owner.verify(&username, &password)))
            .await
            .unwrap_or(false);
        if !verified {
            return Err(failed());
        }
        self.start_session()
    }

    /// Signs the session in `headers` out, if it is live, after checking
    /// its CSRF token.
    fn logout(&self, headers: &HeaderMap) -> Result<(), ApiError> {
        let tokens = self.cookie_tokens(headers);
        if let Some(live) = self.live_session(&tokens) {
            check_csrf(headers, &live.csrf)?;
            self.sessions().revoke(&live.token);
        }
        Ok(())
    }

    /// Runs password hashing on a blocking thread, one at a time.
    async fn hash<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let permit = self
            .hashing
            .clone()
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        tokio::task::spawn_blocking(move || {
            let _hashing = permit;
            work()
        })
        .await
        .map_err(io::Error::other)?
    }

    fn start_session(&self) -> Result<Issued, ApiError> {
        self.sessions()
            .create(Instant::now())
            .map_err(|_| ApiError::new(ErrorCode::Internal, "no session could be created"))
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The first live session among `tokens`.
    fn live_session(&self, tokens: &[String]) -> Option<Live> {
        let mut sessions = self.sessions();
        let now = Instant::now();
        tokens.iter().find_map(|token| {
            let session = sessions.get(token, now)?;
            Some(Live {
                token: token.clone(),
                csrf: session.csrf.clone(),
                end: session.end(now),
            })
        })
    }

    fn cookie_name(&self) -> &'static str {
        if self.secure_cookies {
            SECURE_COOKIE
        } else {
            LOOPBACK_COOKIE
        }
    }

    /// The session tokens in a request's cookies. More than one can arrive
    /// when an old cookie lingers at a broader path.
    fn cookie_tokens(&self, headers: &HeaderMap) -> Vec<String> {
        let name = self.cookie_name();
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(Cookie::split_parse)
            .filter_map(Result::ok)
            .filter(|cookie| cookie.name() == name)
            .map(|cookie| cookie.value().to_owned())
            .collect()
    }

    /// The `Set-Cookie` value that stores `token`, or clears the cookie.
    fn set_cookie(&self, token: Option<&str>) -> HeaderValue {
        let cookie = Cookie::build((self.cookie_name(), token.unwrap_or_default()))
            .path("/")
            .http_only(true)
            .same_site(SameSite::Strict)
            .secure(self.secure_cookies);
        let cookie = match token {
            Some(_) => cookie.max_age(cookie::time::Duration::seconds(
                session::LIFETIME.as_secs().try_into().expect("a week fits"),
            )),
            None => cookie.removal(),
        };
        HeaderValue::from_str(&cookie.build().to_string()).expect("tokens are hex")
    }
}

/// A live session a request presented.
struct Live {
    token: String,
    csrf: String,
    end: SessionEnd,
}

/// Removes the owner account in `auth_dir` and any setup secret, so the
/// next start writes a new setup secret. Refused while a server uses the
/// directory. Returns whether an account existed. Downloads, jobs, and
/// API tokens are untouched.
pub fn reset_owner(auth_dir: &Path) -> Result<bool, BrowserAuthError> {
    let _lock = lock(auth_dir)?;
    let io_err = |path: PathBuf| move |source| BrowserAuthError::Io { path, source };
    let owner = auth_dir.join(OWNER_FILE);
    let existed = owner.try_exists().map_err(io_err(owner.clone()))?;
    secret::remove(&owner).map_err(io_err(owner))?;
    let setup = auth_dir.join(SETUP_FILE);
    secret::remove(&setup).map_err(io_err(setup))?;
    secret::sync_dir(auth_dir).map_err(io_err(auth_dir.to_owned()))?;
    Ok(existed)
}

/// Takes the lock of the auth directory `dir`.
fn lock(dir: &Path) -> Result<File, BrowserAuthError> {
    let path = dir.join(LOCK_FILE);
    let file = secret::open_lock(&path).map_err(|source| BrowserAuthError::Io {
        path: path.clone(),
        source,
    })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(BrowserAuthError::Locked(dir.to_owned())),
        Err(TryLockError::Error(source)) => Err(BrowserAuthError::Io { path, source }),
    }
}

/// The origins of a loopback listener: its own address and `localhost`.
fn loopback_origins(listen: SocketAddr) -> Vec<Url> {
    let port = listen.port();
    let ip = match listen {
        SocketAddr::V4(a) => a.ip().to_string(),
        SocketAddr::V6(a) => format!("[{}]", a.ip()),
    };
    [ip, "localhost".to_owned()]
        .iter()
        .map(|host| Url::parse(&format!("http://{host}:{port}")).expect("a valid origin"))
        .collect()
}

/// Refuses a state-changing request that does not repeat `csrf`.
fn check_csrf(headers: &HeaderMap, csrf: &str) -> Result<(), ApiError> {
    let given = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if secret::digest(given) == secret::digest(csrf) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::PermissionDenied,
            "a valid CSRF token is required",
        ))
    }
}

fn already_set_up() -> ApiError {
    ApiError::new(ErrorCode::NotApplicable, "setup is already complete")
}

#[cfg(test)]
mod tests;
