//! The transport as the engine's provider API client, against a local server
//! over real sockets: what is sent, and what it refuses to accept.

#[path = "../../engine/tests/support/mod.rs"]
mod engine_support;
mod support;

use std::time::Duration;

use bytes::Bytes;
use support::{Raw, Route, Server, config_for};
use url::Url;
use wayfetch_engine::accounts::Secret;
use wayfetch_engine::accounts::provider::{
    ApiClient, ApiError, ApiErrorKind, ApiMethod, ApiRequest, ApiResponse,
};
use wayfetch_net::{HttpConfig, HttpTransport};

fn request(server: &Server, path: &str) -> ApiRequest {
    ApiRequest {
        method: ApiMethod::Get,
        url: Url::parse(&server.url(path)).unwrap(),
        bearer: None,
        form: None,
        deadline: Duration::from_secs(5),
        max_body: 1024,
    }
}

fn answer(head: &str, body: &str) -> Route {
    Route::Raw(Raw::Bytes(Bytes::from(format!(
        "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ))))
}

async fn send(transport: &HttpTransport, request: &ApiRequest) -> Result<ApiResponse, ApiError> {
    transport.send(request).await
}

#[tokio::test]
async fn sends_bearer_and_form_and_reads_the_body() {
    let server = Server::http().await;
    server.route("/api", answer("200 OK", "{\"ok\":true}"));
    let transport = HttpTransport::new(config_for(&server)).unwrap();
    let mut req = request(&server, "/api");
    req.method = ApiMethod::Post;
    req.bearer = Some(Secret::from_text("synthetic-token"));
    req.form = Some(Secret::from_text("a=1&b=2"));
    let response = send(&transport, &req).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body.expose(), b"{\"ok\":true}");
    let logged = &server.log()[0];
    assert_eq!(logged.method, "POST");
    assert_eq!(
        logged.header("authorization"),
        Some("Bearer synthetic-token")
    );
    assert_eq!(
        logged.header("content-type"),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(logged.header("accept-encoding"), Some("identity"));
    assert_eq!(&logged.body[..], b"a=1&b=2");
    // The request's description names no secret.
    let shown = format!("{req:?}");
    assert!(
        !shown.contains("synthetic-token") && !shown.contains("a=1"),
        "{shown}"
    );
}

#[tokio::test]
async fn error_statuses_are_answers() {
    let server = Server::http().await;
    server.route(
        "/api",
        answer("401 Unauthorized", "{\"error\":\"bad_token\"}"),
    );
    let transport = HttpTransport::new(config_for(&server)).unwrap();
    let response = send(&transport, &request(&server, "/api")).await.unwrap();
    assert_eq!(response.status, 401);
}

#[tokio::test]
async fn redirects_are_refused_and_not_followed() {
    let server = Server::http().await;
    server.route("/api", Route::Redirect(302, server.url("/elsewhere")));
    let transport = HttpTransport::new(config_for(&server)).unwrap();
    let mut req = request(&server, "/api");
    req.bearer = Some(Secret::from_text("synthetic-token"));
    let err = send(&transport, &req).await.unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Refused);
    assert_eq!(server.hits("/elsewhere"), 0);
}

#[tokio::test]
async fn oversized_and_encoded_bodies_are_refused() {
    let server = Server::http().await;
    server.route("/large", answer("200 OK", &"x".repeat(2048)));
    server.route(
        "/chunked",
        Route::Raw(Raw::Bytes(Bytes::from(format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n800\r\n{}\r\n0\r\n\r\n",
            "y".repeat(2048)
        )))),
    );
    server.route(
        "/gzip",
        Route::Raw(Raw::Bytes(Bytes::from_static(
            b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\nConnection: close\r\n\r\nab",
        ))),
    );
    let transport = HttpTransport::new(config_for(&server)).unwrap();
    for (path, kind) in [
        ("/large", ApiErrorKind::TooLarge),
        ("/chunked", ApiErrorKind::TooLarge),
        ("/gzip", ApiErrorKind::Refused),
    ] {
        let err = send(&transport, &request(&server, path)).await.unwrap_err();
        assert_eq!(err.kind, kind, "{path}: {}", err.message);
    }
}

#[tokio::test]
async fn deadlines_and_hang_ups_are_temporary() {
    let server = Server::http().await;
    server.route("/hang", Route::Raw(Raw::Hang));
    server.route("/hangup", Route::Raw(Raw::HangUp));
    let transport = HttpTransport::new(config_for(&server)).unwrap();
    let mut req = request(&server, "/hang");
    req.deadline = Duration::from_millis(300);
    let err = send(&transport, &req).await.unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Timeout);
    let err = send(&transport, &request(&server, "/hangup"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Unreachable, "{}", err.message);
}

#[tokio::test]
async fn the_egress_policy_applies() {
    let server = Server::http().await;
    server.route("/api", answer("200 OK", "{}"));
    let transport = HttpTransport::new(HttpConfig::default()).unwrap();
    let mut req = request(&server, "/api?code=synthetic-device-code");
    req.bearer = Some(Secret::from_text("synthetic-token"));
    let err = send(&transport, &req).await.unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Refused);
    assert!(!err.message.contains("synthetic"), "{}", err.message);
    assert!(server.log().is_empty());
}
