use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::*;

const ORIGIN: &str = "https://wayfetch.example.lan";
const LISTEN: &str = "127.0.0.1:7878";
const PASSWORD: &str = "correct horse battery";

fn config(dir: &Path) -> BrowserConfig {
    BrowserConfig {
        auth_dir: dir.join("auth"),
        origins: vec![Url::parse(ORIGIN).unwrap()],
        insecure_loopback: false,
    }
}

fn open(config: &BrowserConfig) -> (BrowserAuth, Option<SetupIssued>) {
    BrowserAuth::open(config, LISTEN.parse().unwrap()).unwrap()
}

fn status(error: ApiError) -> StatusCode {
    error.into_response().status()
}

/// Browser headers for `ORIGIN`, with the session cookie and CSRF token
/// if given.
fn headers(auth: &BrowserAuth, session: Option<&Issued>, csrf: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::HOST,
        HeaderValue::from_static("wayfetch.example.lan"),
    );
    headers.insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
    if let Some(session) = session {
        let cookie = format!("theme=dark; {}={}", auth.cookie_name(), session.token);
        headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
    }
    if let Some(csrf) = csrf {
        headers.insert(CSRF_HEADER, HeaderValue::from_str(csrf).unwrap());
    }
    headers
}

fn read_secret(issued: &SetupIssued) -> String {
    std::fs::read_to_string(&issued.path).unwrap()
}

#[tokio::test]
async fn the_setup_secret_creates_the_owner_once() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config(tmp.path());
    let (auth, issued) = open(&config);
    let issued = issued.expect("no owner yet");
    assert_eq!(issued.valid_for, Duration::from_secs(15 * 60));
    assert!(auth.setup_required());
    let secret = read_secret(&issued);

    let wrong = auth
        .complete_setup(&secret[1..], "owner".into(), PASSWORD.into())
        .await
        .unwrap_err();
    assert_eq!(status(wrong), StatusCode::UNAUTHORIZED);
    let weak = auth
        .complete_setup(&secret, "owner".into(), "short".into())
        .await
        .unwrap_err();
    assert_eq!(status(weak), StatusCode::BAD_REQUEST);
    assert!(auth.setup_required(), "failed setups create nothing");

    let (session, username) = auth
        .complete_setup(&secret, "owner".into(), PASSWORD.into())
        .await
        .unwrap();
    assert_eq!(username, "owner");
    assert!(!auth.setup_required());
    assert!(!issued.path.exists(), "the secret is single-use");
    let grant = auth
        .authenticate(&Method::GET, &headers(&auth, Some(&session), None))
        .unwrap();
    assert_eq!(&*grant.0, Permission::ALL);

    let again = auth
        .complete_setup(&secret, "other".into(), PASSWORD.into())
        .await
        .unwrap_err();
    assert_eq!(status(again), StatusCode::CONFLICT);

    // A restart keeps the owner, writes no secret, and forgets sessions.
    drop(auth);
    let (auth, issued) = open(&config);
    assert!(issued.is_none());
    assert!(!auth.setup_required());
    assert!(
        auth.authenticate(&Method::GET, &headers(&auth, Some(&session), None))
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_setups_create_exactly_one_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let (auth, issued) = open(&config(tmp.path()));
    let secret = read_secret(&issued.unwrap());
    let auth = Arc::new(auth);
    let tasks = (0..4)
        .map(|i| {
            let (auth, secret) = (auth.clone(), secret.clone());
            tokio::spawn(async move {
                auth.complete_setup(&secret, format!("owner{i}"), PASSWORD.into())
                    .await
                    .map(|(_, name)| name)
            })
        })
        .collect::<Vec<_>>();
    let mut created = Vec::new();
    for task in tasks {
        if let Ok(name) = task.await.unwrap() {
            created.push(name);
        }
    }
    assert_eq!(created.len(), 1, "{created:?}");
    let saved = Owner::load(&tmp.path().join("auth")).unwrap().unwrap();
    assert_eq!(saved.username, created[0]);
}

#[tokio::test]
async fn an_expired_setup_secret_is_refused_and_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let (auth, issued) = open(&config(tmp.path()));
    let issued = issued.unwrap();
    let secret = read_secret(&issued);
    auth.setup.lock().await.as_mut().unwrap().expire();
    let expired = auth
        .complete_setup(&secret, "owner".into(), PASSWORD.into())
        .await
        .unwrap_err();
    assert_eq!(status(expired), StatusCode::UNAUTHORIZED);
    assert!(!issued.path.exists());
    assert!(auth.setup_required());
}

#[tokio::test]
async fn sign_in_needs_the_owner_name_and_password_and_says_nothing_more() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut auth, issued) = open(&config(tmp.path()));
    // More attempts than the rate limit allows; that has its own test.
    auth.attempts = RateLimiter::direct(Quota::per_second(NonZeroU32::new(100).unwrap()));
    let early = auth
        .login("owner".into(), PASSWORD.into())
        .await
        .unwrap_err();
    assert_eq!(status(early), StatusCode::UNAUTHORIZED);
    auth.complete_setup(
        &read_secret(&issued.unwrap()),
        "owner".into(),
        PASSWORD.into(),
    )
    .await
    .unwrap();

    let message = |error: ApiError| format!("{error:?}");
    let wrong_password = auth
        .login("owner".into(), "wrong password!".into())
        .await
        .unwrap_err();
    let wrong_name = auth
        .login("nobody".into(), PASSWORD.into())
        .await
        .unwrap_err();
    assert_eq!(message(wrong_password), message(wrong_name));
    let session = auth.login("owner".into(), PASSWORD.into()).await.unwrap();

    // Safe requests need the session and the origin; changes also the
    // CSRF token.
    let get = Method::GET;
    let post = Method::POST;
    assert!(
        auth.authenticate(&get, &headers(&auth, Some(&session), None))
            .is_ok()
    );
    let no_csrf = auth
        .authenticate(&post, &headers(&auth, Some(&session), None))
        .unwrap_err();
    assert_eq!(status(no_csrf), StatusCode::FORBIDDEN);
    let other_csrf = auth
        .authenticate(&post, &headers(&auth, Some(&session), Some(&session.token)))
        .unwrap_err();
    assert_eq!(status(other_csrf), StatusCode::FORBIDDEN);
    assert!(
        auth.authenticate(&post, &headers(&auth, Some(&session), Some(&session.csrf)))
            .is_ok()
    );
    let mut cross = headers(&auth, Some(&session), Some(&session.csrf));
    cross.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://evil.test"),
    );
    assert_eq!(
        status(auth.authenticate(&post, &cross).unwrap_err()),
        StatusCode::FORBIDDEN
    );
    let no_cookie = auth
        .authenticate(&get, &headers(&auth, None, None))
        .unwrap_err();
    assert_eq!(status(no_cookie), StatusCode::UNAUTHORIZED);

    // Signing out needs the CSRF token and ends only that session.
    let other = auth.login("owner".into(), PASSWORD.into()).await.unwrap();
    assert!(auth.logout(&headers(&auth, Some(&session), None)).is_err());
    auth.logout(&headers(&auth, Some(&session), Some(&session.csrf)))
        .unwrap();
    assert_eq!(
        status(
            auth.authenticate(&get, &headers(&auth, Some(&session), None))
                .unwrap_err()
        ),
        StatusCode::UNAUTHORIZED
    );
    assert!(
        auth.authenticate(&get, &headers(&auth, Some(&other), None))
            .is_ok()
    );
}

#[tokio::test]
async fn attempts_are_rate_limited() {
    let tmp = tempfile::tempdir().unwrap();
    let (auth, issued) = open(&config(tmp.path()));
    let secret = read_secret(&issued.unwrap());
    for _ in 0..ATTEMPTS_PER_MINUTE.get() - 1 {
        let error = auth
            .complete_setup("wrong", "owner".into(), PASSWORD.into())
            .await
            .unwrap_err();
        assert_eq!(status(error), StatusCode::UNAUTHORIZED);
    }
    auth.login("owner".into(), PASSWORD.into())
        .await
        .unwrap_err();
    // Even the right secret waits now.
    let limited = auth
        .complete_setup(&secret, "owner".into(), PASSWORD.into())
        .await
        .unwrap_err()
        .into_response();
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = limited.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=12).contains(&retry), "{retry}");
    assert!(auth.setup_required());
}

#[test]
fn one_server_uses_an_auth_directory_and_reset_waits_for_it() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config(tmp.path());
    let (auth, _) = open(&config);
    assert!(matches!(
        BrowserAuth::open(&config, LISTEN.parse().unwrap()),
        Err(BrowserAuthError::Locked(_))
    ));
    assert!(matches!(
        reset_owner(&config.auth_dir),
        Err(BrowserAuthError::Locked(_))
    ));
    drop(auth);

    Owner::new("owner".into(), PASSWORD)
        .unwrap()
        .publish(&config.auth_dir)
        .unwrap();
    assert!(reset_owner(&config.auth_dir).unwrap());
    assert!(!reset_owner(&config.auth_dir).unwrap());
    let (auth, issued) = open(&config);
    assert!(issued.is_some() && auth.setup_required());
}

#[cfg(unix)]
#[test]
fn the_auth_directory_is_created_owner_only_and_checked() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = config(tmp.path());
    drop(open(&config));
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&config.auth_dir), 0o700);
    assert_eq!(mode(&config.auth_dir.join(SETUP_FILE)), 0o600);
    std::fs::set_permissions(&config.auth_dir, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(matches!(
        BrowserAuth::open(&config, LISTEN.parse().unwrap()),
        Err(BrowserAuthError::Exposed(_))
    ));
}

#[test]
fn plain_http_cookies_only_for_loopback_development() {
    let tmp = tempfile::tempdir().unwrap();
    let secure = open(&config(tmp.path())).0;
    let cookie = secure.set_cookie(Some("abc"));
    let cookie = cookie.to_str().unwrap();
    for part in [
        "__Host-wayfetch-session=abc",
        "HttpOnly",
        "SameSite=Strict",
        "Secure",
        "Path=/",
        "Max-Age=604800",
    ] {
        assert!(cookie.contains(part), "{cookie}");
    }
    assert!(!cookie.contains("Domain"), "{cookie}");
    let cleared = secure.set_cookie(None);
    assert!(
        cleared.to_str().unwrap().contains("Max-Age=0"),
        "{cleared:?}"
    );
    drop(secure);

    let loopback = BrowserConfig {
        auth_dir: tmp.path().join("auth"),
        origins: Vec::new(),
        insecure_loopback: true,
    };
    assert!(matches!(
        BrowserAuth::open(&loopback, "0.0.0.0:7878".parse().unwrap()),
        Err(BrowserAuthError::NotLoopback(_))
    ));
    let dev = open(&loopback).0;
    assert_eq!(
        dev.origins(),
        ["http://127.0.0.1:7878", "http://localhost:7878"]
    );
    let cookie = dev.set_cookie(Some("abc"));
    let cookie = cookie.to_str().unwrap();
    assert!(cookie.starts_with("wayfetch-session=abc"), "{cookie}");
    assert!(!cookie.contains("Secure"), "{cookie}");
    drop(dev);

    let lan = BrowserConfig {
        origins: vec![Url::parse("http://192.168.1.2:7878").unwrap()],
        ..loopback
    };
    assert!(matches!(
        BrowserAuth::open(&lan, LISTEN.parse().unwrap()),
        Err(BrowserAuthError::BadOrigin { .. })
    ));
}
