//! Downloading through a Real-Debrid account against a local stand-in for
//! its API, through the real HTTP transport: generating a link, refreshing
//! for it, every refusal, a lost answer, and reading the account's record.
//! Fixture-tested only: nothing here reaches Real-Debrid.

mod support;

use std::sync::Arc;

use support::{Answer, DOWNLOADS_PATH, SIGNATURE, Server, TOKEN_PATH, UNRESTRICT_PATH};
use url::Url;
use wayfetch_adapters::Endpoint;
use wayfetch_adapters::real_debrid::RealDebrid;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    AccountProvider, PollOutcome, Reconciled, Unrestricted, WithCredentials,
};
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::time::UtcTimestamp;
use wayfetch_net::{HttpConfig, HttpTransport};

const LINK: &str = "https://hoster.example/file/abc123";

fn provider(server: &Server) -> RealDebrid {
    let mut config = HttpConfig::default();
    config
        .egress
        .allowed_networks
        .push("127.0.0.1/32".parse().unwrap());
    RealDebrid::new(
        Arc::new(HttpTransport::new(config).unwrap()),
        Endpoint::custom(&server.base()).unwrap(),
    )
}

async fn sign_in(rd: &RealDebrid) -> Secret {
    let started = AccountProvider::begin(rd).await.unwrap();
    loop {
        if let PollOutcome::Authorized { credential, .. } =
            AccountProvider::poll(rd, &started.pending).await.unwrap()
        {
            return credential;
        }
    }
}

async fn unrestrict(rd: &RealDebrid, credential: &Secret) -> WithCredentials<Unrestricted> {
    AccountProvider::unrestrict(rd, credential, &Url::parse(LINK).unwrap()).await
}

fn refused(result: &Unrestricted) -> FailureCode {
    match result {
        Unrestricted::Refused { code, .. } => *code,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_link_is_generated_once_with_the_bearer_token() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let answer = unrestrict(&rd, &credential).await;
    assert!(answer.refreshed.is_none());
    let Unrestricted::Generated(link) = &answer.result else {
        panic!("{:?}", answer.result)
    };
    assert_eq!(link.filename.as_deref(), Some("Synthetic File.bin"));
    assert_eq!(link.size, Some(34));
    assert!(link.url.as_str().contains(SIGNATURE));
    assert!(link.url.as_str().starts_with(&server.base()));
    // The link never prints.
    assert!(!format!("{answer:?}").contains(SIGNATURE));
    let sent = server
        .log()
        .into_iter()
        .find(|r| r.path == UNRESTRICT_PATH)
        .unwrap();
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.form.get("link").map(String::as_str), Some(LINK));
    assert_eq!(sent.form.len(), 1, "no password or remote traffic");
    assert_eq!(
        sent.authorization.as_deref(),
        Some("Bearer synthetic-access-1")
    );
    assert_eq!(server.hits(UNRESTRICT_PATH), 1);
}

#[tokio::test]
async fn an_unknown_size_is_none() {
    let server = Server::start().await;
    server.with(|rd| rd.filesize = 0);
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let Unrestricted::Generated(link) = unrestrict(&rd, &credential).await.result else {
        panic!()
    };
    assert_eq!(link.size, None);
}

#[tokio::test]
async fn an_expired_or_refused_token_is_refreshed_once_before_asking() {
    let server = Server::start().await;
    server.with(|rd| rd.token_lifetime = 1);
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    // Expired: refreshed first, so the request is sent once.
    let answer = unrestrict(&rd, &credential).await;
    assert!(matches!(answer.result, Unrestricted::Generated(_)));
    let refreshed = answer.refreshed.expect("new tokens");
    assert_eq!(server.hits(TOKEN_PATH), 2);
    assert_eq!(server.hits(UNRESTRICT_PATH), 1);

    // Refused with 401 before anything was generated: refreshed, then sent
    // once more.
    server.with(|rd| {
        rd.token_lifetime = 3600;
        rd.valid_access = Some("rotated-elsewhere".into());
    });
    let stored: serde_json::Value = serde_json::from_slice(refreshed.expose()).unwrap();
    let mut fresh = stored.clone();
    fresh["expires_at"] = serde_json::json!(i64::MAX / 2);
    let fresh = Secret::new(serde_json::to_vec(&fresh).unwrap());
    let answer = unrestrict(&rd, &fresh).await;
    assert!(
        matches!(answer.result, Unrestricted::Generated(_)),
        "{:?}",
        answer.result
    );
    assert!(answer.refreshed.is_some());
    assert_eq!(server.hits(UNRESTRICT_PATH), 3);
    assert_eq!(server.with(|rd| rd.generated.len()), 2);
}

#[tokio::test]
async fn every_refusal_says_what_to_do_and_is_not_repeated() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let cases = [
        (403, 16, FailureCode::HostUnavailableViaProvider),
        (503, 19, FailureCode::HostUnavailableViaProvider),
        (403, 20, FailureCode::HostUnavailableViaProvider),
        (503, 24, FailureCode::ResourceDeleted),
        (403, 23, FailureCode::QuotaExhausted),
        (403, 36, FailureCode::QuotaExhausted),
        (403, 14, FailureCode::InvalidCredentials),
        (403, 22, FailureCode::InvalidCredentials),
        (400, 2, FailureCode::AdapterParseFailure),
        (503, 25, FailureCode::RequestTimeout),
    ];
    for (n, (status, code, expected)) in cases.into_iter().enumerate() {
        server.with(|rd| {
            rd.once.insert(
                UNRESTRICT_PATH,
                vec![Answer::json(
                    status,
                    format!(r#"{{"error":"synthetic","error_code":{code}}}"#),
                )],
            );
        });
        let answer = unrestrict(&rd, &credential).await;
        assert_eq!(refused(&answer.result), expected, "{code}");
        let Unrestricted::Refused { message, .. } = &answer.result else {
            unreachable!()
        };
        assert!(message.contains("Real-Debrid"), "{message}");
        assert_eq!(server.hits(UNRESTRICT_PATH), n + 1, "sent once");
    }
}

#[tokio::test]
async fn a_rate_limit_says_when_to_try_again() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    for answer in [
        Answer::json(429, r#"{"error":"too_many_requests","error_code":34}"#),
        Answer::json(429, "rate limited"),
    ] {
        server.with(|rd| rd.once.insert(UNRESTRICT_PATH, vec![answer]));
        let result = unrestrict(&rd, &credential).await.result;
        assert!(
            matches!(
                result,
                Unrestricted::Refused {
                    code: FailureCode::QuotaExhausted,
                    retry_after: Some(_),
                    ..
                }
            ),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn a_lost_or_unreadable_answer_is_ambiguous_and_never_resent() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let lost = [
        // Headers, then the connection drops mid-body.
        Answer::Raw(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 400\r\n\r\n{\"download\""
                .to_vec(),
        ),
        // The connection drops before any answer.
        Answer::Raw(Vec::new()),
        Answer::json(200, "not json"),
        // Answered, but encoded or redirected: refused unread, after the
        // request reached Real-Debrid.
        Answer::Raw(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\n{}"
                .to_vec(),
        ),
        Answer::Raw(
            b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere.example/\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
        ),
        Answer::json(500, r#"{"error":"internal","error_code":-1}"#),
        // A server error that does not say why.
        Answer::json(503, "Service Unavailable"),
        Answer::json(502, r#"{"error":"bad_gateway"}"#),
    ];
    for (n, answer) in lost.into_iter().enumerate() {
        server.with(|rd| rd.once.insert(UNRESTRICT_PATH, vec![answer]));
        let result = unrestrict(&rd, &credential).await.result;
        assert!(
            matches!(result, Unrestricted::Ambiguous(_)),
            "{n}: {result:?}"
        );
        assert_eq!(server.hits(UNRESTRICT_PATH), n + 1, "sent once");
    }
}

#[tokio::test]
async fn several_results_and_unsafe_links_are_refused() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let several = r#"{"id":"X","filename":"v.mp4","filesize":1,"link":"x","host":"h","download":"https://cdn.example/v.mp4","type":"720p","alternative":[{"id":"Y","filename":"v.mp4","download":"https://cdn.example/v2.mp4","type":"480p"}]}"#;
    server.with(|rd| {
        rd.once
            .insert(UNRESTRICT_PATH, vec![Answer::json(200, several)])
    });
    let result = unrestrict(&rd, &credential).await.result;
    assert_eq!(refused(&result), FailureCode::UnsupportedCapability);
    for download in [
        "http://cdn.example/f.bin",
        "https://user:pw@cdn.example/f.bin",
        "ftp://cdn.example/f.bin",
        "not a link",
    ] {
        let body = serde_json::json!({"filename": "f", "filesize": 1, "download": download});
        server.with(|rd| {
            rd.once
                .insert(UNRESTRICT_PATH, vec![Answer::json(200, body.to_string())])
        });
        let result = unrestrict(&rd, &credential).await.result;
        assert_eq!(
            refused(&result),
            FailureCode::AdapterParseFailure,
            "{download}"
        );
    }
}

#[tokio::test]
async fn the_record_shows_links_generated_since_the_request() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let link = Url::parse(LINK).unwrap();
    let find = |since: i64| {
        let (rd, credential, link) = (&rd, &credential, &link);
        async move {
            AccountProvider::find_generated(
                rd,
                credential,
                link,
                UtcTimestamp::from_unix_millis(since),
            )
            .await
            .result
        }
    };
    // Empty: Real-Debrid answers 204.
    assert_eq!(find(0).await, Reconciled::NotFound);

    // Generated, but the answer was lost.
    server.with(|rd| rd.generated_at = "2026-10-16T10:00:00.000Z".into());
    server.with(|rd| {
        rd.once.insert(UNRESTRICT_PATH, vec![]);
    });
    unrestrict(&rd, &credential).await;
    let at = 1_792_144_800_000;
    let Reconciled::Found(found) = find(at - 1000).await else {
        panic!()
    };
    assert!(found.url.as_str().contains(&format!("{SIGNATURE}-1")));
    assert_eq!(found.filename.as_deref(), Some("Synthetic File.bin"));
    // Generated before the request: not its outcome.
    assert_eq!(find(at + 1000).await, Reconciled::NotFound);
    // Another link's.
    let other = Url::parse("https://hoster.example/file/other").unwrap();
    let result = AccountProvider::find_generated(
        &rd,
        &credential,
        &other,
        UtcTimestamp::from_unix_millis(0),
    )
    .await
    .result;
    assert_eq!(result, Reconciled::NotFound);
    // An unreadable record proves nothing.
    server.with(|rd| {
        rd.once
            .insert(DOWNLOADS_PATH, vec![Answer::json(500, "{}")])
    });
    assert!(matches!(find(0).await, Reconciled::Unknown(_)));
    server.with(|rd| {
        rd.once
            .insert(DOWNLOADS_PATH, vec![Answer::json(200, "[{")])
    });
    assert!(matches!(find(0).await, Reconciled::Unknown(_)));
    // Reading the record never generates anything.
    assert_eq!(server.hits(UNRESTRICT_PATH), 1);
    let read = server
        .log()
        .into_iter()
        .find(|r| r.path == DOWNLOADS_PATH)
        .unwrap();
    assert_eq!(read.method, "GET");
    assert_eq!(read.query.get("limit").map(String::as_str), Some("100"));
}

#[tokio::test]
async fn only_one_exact_entry_is_taken_as_the_outcome() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    let since = UtcTimestamp::from_unix_millis(1_792_144_000_000);
    let entry = |link: &str, n: u32| support::Generated {
        link: link.into(),
        path: format!("/d/entry{n}/f"),
        generated: "2026-10-16T10:00:00.000Z".into(),
    };
    let original = Url::parse(LINK).unwrap();
    let find = || AccountProvider::find_generated(&rd, &credential, &original, since);

    // The same path on another port or scheme is another origin: never
    // taken, and not proof that nothing was generated either.
    for similar in [
        "https://hoster.example:8443/file/abc123",
        "http://hoster.example/file/abc123",
        "https://hoster.example/file/abc123?mirror=2",
    ] {
        server.with(|rd| rd.generated = vec![entry(similar, 1)]);
        let result = find().await.result;
        assert!(
            matches!(result, Reconciled::Inconclusive(_)),
            "{similar}: {result:?}"
        );
    }

    // Two entries for exactly this link: either may be another
    // request's, so neither is chosen.
    server.with(|rd| rd.generated = vec![entry(LINK, 1), entry(LINK, 2)]);
    let result = find().await.result;
    assert!(matches!(result, Reconciled::Inconclusive(_)), "{result:?}");

    // One exact entry beside other links' entries: taken.
    server.with(|rd| {
        rd.generated = vec![
            entry("https://hoster.example/file/other", 1),
            entry(LINK, 2),
        ]
    });
    let Reconciled::Found(found) = find().await.result else {
        panic!()
    };
    assert!(
        found.url.path().ends_with("/d/entry2/f"),
        "{}",
        found.url.path()
    );
    assert_eq!(server.hits(UNRESTRICT_PATH), 0);
}

#[tokio::test]
async fn a_full_page_that_does_not_reach_the_request_proves_nothing() {
    let server = Server::start().await;
    let rd = provider(&server);
    let credential = sign_in(&rd).await;
    server.with(|rd| {
        for n in 0..100 {
            rd.generated.push(support::Generated {
                link: format!("https://hoster.example/file/{n}"),
                path: format!("/d/x{n}/f"),
                generated: "2026-10-16T10:00:00.000Z".into(),
            });
        }
    });
    let result = AccountProvider::find_generated(
        &rd,
        &credential,
        &Url::parse(LINK).unwrap(),
        UtcTimestamp::from_unix_millis(1_792_144_000_000),
    )
    .await
    .result;
    assert!(matches!(result, Reconciled::Inconclusive(_)), "{result:?}");
}
