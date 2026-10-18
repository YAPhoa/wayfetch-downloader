//! The Real-Debrid integration against a local stand-in for its API, through
//! the real HTTP transport. Fixture-tested only: nothing here reaches
//! Real-Debrid.

mod support;

use std::sync::Arc;
use std::time::Duration;

use support::{
    Answer, CLIENT_SECRET, CREDENTIALS_PATH, DEVICE_CODE, DEVICE_PATH, Server, TOKEN_PATH,
    USER_CODE, USER_PATH,
};
use wayfetch_adapters::Endpoint;
use wayfetch_adapters::real_debrid::{OFFICIAL_API, OPEN_SOURCE_CLIENT_ID, RealDebrid};
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, Checked, PollOutcome, Profile, ProviderError,
};
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_engine::protocol::account::AccountPlan;
use wayfetch_net::{HttpConfig, HttpTransport};

fn provider(server: &Server) -> RealDebrid {
    let mut config = HttpConfig::default();
    config
        .egress
        .allowed_networks
        .push("127.0.0.1/32".parse().unwrap());
    let transport = HttpTransport::new(config).unwrap();
    RealDebrid::new(
        Arc::new(transport),
        Endpoint::custom(&server.base()).unwrap(),
    )
}

/// Signs in through the device flow and returns the stored credential.
async fn sign_in(rd: &RealDebrid) -> (Secret, Profile) {
    let started = AccountProvider::begin(rd).await.unwrap();
    loop {
        match AccountProvider::poll(rd, &started.pending).await.unwrap() {
            PollOutcome::Pending => {}
            PollOutcome::Authorized {
                credential,
                profile,
            } => return (credential, profile),
            other => panic!("{other:?}"),
        }
    }
}

/// A check that confirmed the account.
#[derive(Debug)]
struct Found {
    profile: Profile,
    refreshed: Option<Secret>,
}

async fn check(rd: &RealDebrid, credential: &Secret) -> Result<Found, ProviderError> {
    let Checked { result, refreshed } = AccountProvider::check(rd, credential).await;
    result.map(|profile| Found { profile, refreshed })
}

/// Text that must never be shown: everything Real-Debrid hands out that
/// grants access.
const SECRETS: [&str; 4] = [
    DEVICE_CODE,
    CLIENT_SECRET,
    "synthetic-access-",
    "synthetic-refresh-",
];

fn assert_no_secret(text: &str) {
    for secret in SECRETS {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}

#[tokio::test]
async fn signs_in_through_the_device_flow() {
    let server = Server::start().await;
    let rd = provider(&server);
    let started = AccountProvider::begin(&rd).await.unwrap();
    assert_eq!(started.user_code, USER_CODE);
    assert_eq!(
        started.verification_url.as_str(),
        format!("{}device", server.base())
    );
    assert_eq!(started.interval, Duration::from_secs(1));
    assert_eq!(started.expires_in, Duration::from_secs(600));
    assert_no_secret(&format!("{started:?}"));

    let first = AccountProvider::poll(&rd, &started.pending).await.unwrap();
    assert!(matches!(first, PollOutcome::Pending), "{first:?}");
    let PollOutcome::Authorized {
        credential,
        profile,
    } = AccountProvider::poll(&rd, &started.pending).await.unwrap()
    else {
        panic!("not authorized");
    };
    assert_eq!(profile.user_id.as_deref(), Some("4242"));
    assert_eq!(profile.username.as_deref(), Some("synthetic-user"));
    assert_eq!(profile.plan, Some(AccountPlan::Premium));
    let left = profile.expires_at.unwrap().unix_millis()
        - UtcTimestamp::from_system_time(std::time::SystemTime::now()).unix_millis();
    assert!((86_000_000..=86_400_000).contains(&left), "{left}");

    // The credential is what Real-Debrid issued to this person, bound to
    // the API that issued it.
    let stored: serde_json::Value = serde_json::from_slice(credential.expose()).unwrap();
    assert_eq!(stored["client_id"], support::CLIENT_ID);
    assert_eq!(stored["access_token"], "synthetic-access-1");
    assert_eq!(stored["refresh_token"], "synthetic-refresh-1");
    assert_eq!(
        stored["origin"],
        server.base().trim_end_matches('/'),
        "{stored}"
    );

    let log = server.log();
    let device = &log[0];
    assert_eq!(device.path, DEVICE_PATH);
    assert_eq!(device.query["client_id"], OPEN_SOURCE_CLIENT_ID);
    assert_eq!(device.query["new_credentials"], "yes");
    let token = log.iter().find(|r| r.path == TOKEN_PATH).unwrap();
    assert_eq!(token.method, "POST");
    assert_eq!(token.form["code"], DEVICE_CODE);
    assert_eq!(token.form["client_secret"], CLIENT_SECRET);
    assert!(token.query.is_empty(), "secrets stay out of the URL");
    let user = log.iter().find(|r| r.path == USER_PATH).unwrap();
    assert_eq!(
        user.authorization.as_deref(),
        Some("Bearer synthetic-access-1")
    );
}

#[tokio::test]
async fn polling_slows_down_and_reports_outages() {
    let server = Server::start().await;
    let rd = provider(&server);
    let started = AccountProvider::begin(&rd).await.unwrap();
    server.with(|s| {
        s.once.insert(
            CREDENTIALS_PATH,
            vec![
                Answer::json(429, r#"{"error":"too_many_requests","error_code":34}"#),
                Answer::json(503, r#"{"error":"service_unavailable","error_code":25}"#),
            ],
        )
    });
    let outcome = AccountProvider::poll(&rd, &started.pending).await.unwrap();
    assert!(matches!(outcome, PollOutcome::SlowDown), "{outcome:?}");
    let err = AccountProvider::poll(&rd, &started.pending)
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Unavailable(_)), "{err:?}");
}

#[tokio::test]
async fn sign_in_answers_are_validated() {
    let server = Server::start().await;
    let rd = provider(&server);
    let bad = [
        r#"{"device_code":"x","user_code":"<script>","interval":5,"expires_in":600,"verification_url":"https://rd.example/device"}"#,
        r#"{"device_code":"x","user_code":"ABCD","interval":5,"expires_in":600,"verification_url":"javascript:alert(1)"}"#,
        r#"{"device_code":"x","user_code":"ABCD","interval":5,"expires_in":600,"verification_url":"http://elsewhere.example/device"}"#,
        r#"{"device_code":"","user_code":"ABCD","interval":5,"expires_in":600,"verification_url":"https://rd.example/device"}"#,
        r#"{"device_code":"x","user_code":"ABCD","interval":5,"expires_in":0,"verification_url":"https://rd.example/device"}"#,
        r#"{"device_code":"x"}"#,
        "not json",
    ];
    for body in bad {
        server.with(|s| s.once.insert(DEVICE_PATH, vec![Answer::json(200, body)]));
        let err = AccountProvider::begin(&rd).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::Malformed(_)),
            "{body}: {err:?}"
        );
    }
    // A fast or slow interval is clamped.
    server.with(|s| {
        s.once.insert(
            DEVICE_PATH,
            vec![Answer::json(
                200,
                r#"{"device_code":"x","user_code":"ABCD","interval":0,"expires_in":600,"verification_url":"https://rd.example/device"}"#,
            )],
        )
    });
    let started = AccountProvider::begin(&rd).await.unwrap();
    assert_eq!(started.interval, Duration::from_secs(1));
}

#[tokio::test]
async fn checks_and_refreshes_tokens() {
    let server = Server::start().await;
    let rd = provider(&server);
    let (credential, _) = sign_in(&rd).await;

    // A valid token is used as it is.
    let checked = check(&rd, &credential).await.unwrap();
    assert!(checked.refreshed.is_none());
    assert_eq!(checked.profile.username.as_deref(), Some("synthetic-user"));
    assert_eq!(server.hits(TOKEN_PATH), 1);

    // Real-Debrid stops accepting the token early: refreshed once.
    server.with(|s| s.valid_access = Some("revoked".into()));
    let checked = check(&rd, &credential).await.unwrap();
    let refreshed = checked.refreshed.expect("new tokens");
    let stored: serde_json::Value = serde_json::from_slice(refreshed.expose()).unwrap();
    assert_eq!(stored["access_token"], "synthetic-access-2");
    assert_eq!(stored["refresh_token"], "synthetic-refresh-2");
    let token = server
        .log()
        .into_iter()
        .filter(|r| r.path == TOKEN_PATH)
        .nth(1)
        .unwrap();
    assert_eq!(token.form["code"], "synthetic-refresh-1");

    // The old credential's refresh token is no longer accepted.
    server.with(|s| s.valid_access = Some("revoked".into()));
    let err = check(&rd, &credential).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::InvalidCredentials(_)),
        "{err:?}"
    );
    assert_no_secret(&err.to_string());
}

#[tokio::test]
async fn refreshed_tokens_survive_a_failed_account_check() {
    let server = Server::start().await;
    let rd = provider(&server);
    let (credential, _) = sign_in(&rd).await;

    // The token is refused, refreshed, and then Real-Debrid fails.
    server.with(|s| {
        s.once.insert(
            USER_PATH,
            vec![Answer::json(401, "{}"), Answer::json(503, "{}")],
        )
    });
    let Checked { result, refreshed } = AccountProvider::check(&rd, &credential).await;
    assert!(
        matches!(result, Err(ProviderError::Unavailable(_))),
        "{result:?}"
    );
    let refreshed = refreshed.expect("the new tokens are handed back");
    let stored: serde_json::Value = serde_json::from_slice(refreshed.expose()).unwrap();
    assert_eq!(stored["refresh_token"], "synthetic-refresh-2");

    // Real-Debrid rotated the refresh token: only the new tokens work.
    let found = check(&rd, &refreshed).await.unwrap();
    assert_eq!(found.profile.username.as_deref(), Some("synthetic-user"));
    server.with(|s| s.valid_access = Some("revoked".into()));
    let err = check(&rd, &credential).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::InvalidCredentials(_)),
        "{err:?}"
    );
}

#[tokio::test]
async fn an_expired_token_is_refreshed_before_use() {
    let server = Server::start().await;
    server.with(|s| s.token_lifetime = 0);
    let rd = provider(&server);
    let (credential, _) = sign_in(&rd).await;
    let users = server.hits(USER_PATH);
    let checked = check(&rd, &credential).await.unwrap();
    assert!(checked.refreshed.is_some());
    // Refreshed first, so the expired token was never sent.
    assert_eq!(server.hits(TOKEN_PATH), 2);
    assert_eq!(server.hits(USER_PATH), users + 1);
    let last = server.log().pop().unwrap();
    assert_eq!(
        last.authorization.as_deref(),
        Some("Bearer synthetic-access-2")
    );
}

#[tokio::test]
async fn refusals_mean_signing_in_again() {
    let server = Server::start().await;
    let rd = provider(&server);
    let (credential, _) = sign_in(&rd).await;

    // A locked or forbidden account.
    server.with(|s| {
        s.once.insert(
            USER_PATH,
            vec![Answer::json(
                403,
                r#"{"error":"account_locked","error_code":22}"#,
            )],
        )
    });
    let err = check(&rd, &credential).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::InvalidCredentials(_)),
        "{err:?}"
    );

    // A token that stays refused after refreshing.
    server.with(|s| {
        s.once.insert(
            USER_PATH,
            vec![
                Answer::json(401, r#"{"error":"bad_token","error_code":8}"#),
                Answer::json(401, r#"{"error":"bad_token","error_code":8}"#),
            ],
        )
    });
    let err = check(&rd, &credential).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::InvalidCredentials(_)),
        "{err:?}"
    );
    assert_eq!(server.hits(TOKEN_PATH), 2, "refreshed only once");

    // A credential this version cannot read, or from another address, is
    // never sent anywhere.
    let before = server.log().len();
    let mut stored: serde_json::Value = serde_json::from_slice(credential.expose()).unwrap();
    stored["origin"] = "https://elsewhere.example".into();
    let moved = Secret::new(serde_json::to_vec(&stored).unwrap());
    for credential in [moved, Secret::from_text("{}"), Secret::from_text("garbage")] {
        let err = check(&rd, &credential).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::InvalidCredentials(_)),
            "{err:?}"
        );
    }
    assert_eq!(server.log().len(), before);
}

#[tokio::test]
async fn outages_and_malformed_answers_are_not_refusals() {
    let server = Server::start().await;
    let rd = provider(&server);
    let (credential, _) = sign_in(&rd).await;
    let oversized = format!(
        r#"{{"id":1,"username":"{}","type":"free"}}"#,
        "x".repeat(70 * 1024)
    );
    let cases = [
        (
            Answer::json(503, r#"{"error":"service_unavailable","error_code":25}"#),
            "unavailable",
        ),
        (
            Answer::json(429, r#"{"error":"too_many_requests","error_code":34}"#),
            "unavailable",
        ),
        (Answer::Raw(Vec::new()), "unavailable"),
        (Answer::json(200, "{not json"), "malformed"),
        (Answer::json(200, oversized), "malformed"),
        (Answer::json(418, "{}"), "malformed"),
    ];
    for (answer, expected) in cases {
        let shown = format!("{answer:?}");
        server.with(|s| s.once.insert(USER_PATH, vec![answer]));
        let err = check(&rd, &credential).await.unwrap_err();
        let kind = match &err {
            ProviderError::Unavailable(_) => "unavailable",
            ProviderError::Malformed(_) => "malformed",
            ProviderError::InvalidCredentials(_) => "invalid",
        };
        assert_eq!(kind, expected, "{}: {err:?}", &shown[..shown.len().min(80)]);
        assert_no_secret(&err.to_string());
    }
    // The token refresh failing temporarily is not a refusal either.
    server.with(|s| {
        s.valid_access = Some("revoked".into());
        s.once.insert(
            TOKEN_PATH,
            vec![Answer::json(502, r#"{"error":"bad_gateway"}"#)],
        );
    });
    let err = check(&rd, &credential).await.unwrap_err();
    assert!(matches!(err, ProviderError::Unavailable(_)), "{err:?}");
}

#[tokio::test]
async fn free_accounts_have_no_expiry() {
    let server = Server::start().await;
    server.with(|s| {
        s.user = r#"{"id":7,"username":"synthetic-free","type":"free","premium":0,"expiration":"1970-01-01T00:00:00.000Z"}"#.into()
    });
    let rd = provider(&server);
    let (_, profile) = sign_in(&rd).await;
    assert_eq!(profile.plan, Some(AccountPlan::Free));
    assert_eq!(profile.expires_at, None);
}

#[test]
fn downloads_are_supported() {
    let support = RealDebrid::new(
        Arc::new(HttpTransport::new(HttpConfig::default()).unwrap()),
        Endpoint::official(OFFICIAL_API),
    )
    .support();
    assert!(support.downloads);
    assert_eq!(support.service.as_str(), "real-debrid");
}

#[tokio::test]
async fn supported_domains_use_the_documented_public_bounded_endpoint() {
    let server = Server::start().await;
    let rd = provider(&server);
    let hosts = AccountProvider::supported_hosts(&rd).await.unwrap();
    assert!(hosts.supports(&url::Url::parse("https://sub.hoster.example/file").unwrap()));
    let log = server.log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].method, "GET");
    assert_eq!(log[0].path, "/rest/1.0/hosts/domains");
    assert!(log[0].authorization.is_none());
    assert!(log[0].form.is_empty());
}

#[tokio::test]
async fn unsupported_domain_answers_and_outages_remain_unknown() {
    for (status, body) in [
        (503, "{}".to_owned()),
        (200, "{}".to_owned()),
        (200, r#"["*.example"]"#.to_owned()),
        (
            200,
            serde_json::to_string(&vec!["hoster.example"; 1025]).unwrap(),
        ),
        (
            200,
            serde_json::to_string(&vec!["hoster.example"; 8192]).unwrap(),
        ),
    ] {
        let server = Server::start().await;
        server.with(|state| {
            state
                .always
                .insert("/rest/1.0/hosts/domains", Answer::json(status, body));
        });
        assert!(
            AccountProvider::supported_hosts(&provider(&server))
                .await
                .is_err()
        );
        assert_eq!(server.log().len(), 1, "never silently retry");
    }
}
