//! The `/v1/auth` routes, for browsers only:
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /v1/auth/session` | whether setup is pending; the session's owner and CSRF token |
//! | `POST /v1/auth/setup` | create the owner with the setup secret, and sign in |
//! | `POST /v1/auth/login` | sign in |
//! | `POST /v1/auth/logout` | sign out (needs the CSRF token) |
//!
//! Each checks the request's `Host` and `Origin` first. Responses,
//! errors included, are never cached ([`no_store`]). Without browser access configured, each answers
//! `not_found`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use wayfetch_engine::protocol::{ErrorCode, Permission};

use super::session::Issued;
use super::{BrowserAuth, OWNER};
use crate::error::ApiError;
use crate::routes::{AppState, JsonBody};

/// What a browser learns about its session.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SessionStatus {
    /// No owner account exists yet.
    setup_required: bool,
    /// The request carried a live session.
    authenticated: bool,
    /// The owner's name, when authenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    username: Option<String>,
    /// The value state-changing requests repeat in `X-CSRF-Token`, when
    /// authenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    csrf_token: Option<String>,
    /// What the session may do, when authenticated, so the UI offers only
    /// what will be allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    permissions: Option<&'static [Permission]>,
}

/// The body of `POST /v1/auth/setup`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SetupRequest {
    setup_secret: String,
    username: String,
    password: String,
}

/// The body of `POST /v1/auth/login`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LoginRequest {
    username: String,
    password: String,
}

/// The session of the request, if any.
pub async fn session(
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let browser = checked(&app, &method, &headers)?;
    let tokens = browser.cookie_tokens(&headers);
    let status = match (browser.live_session(&tokens), browser.owner()) {
        (Some(live), Some(owner)) => SessionStatus {
            setup_required: false,
            authenticated: true,
            username: Some(owner.username.clone()),
            csrf_token: Some(live.csrf),
            permissions: Some(OWNER),
        },
        (_, owner) => SessionStatus {
            setup_required: owner.is_none(),
            authenticated: false,
            username: None,
            csrf_token: None,
            permissions: None,
        },
    };
    Ok(Json(status).into_response())
}

/// Creates the owner account and signs its browser in.
pub async fn setup(
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
    JsonBody(request): JsonBody<SetupRequest>,
) -> Result<Response, ApiError> {
    let browser = checked(&app, &method, &headers)?;
    let (issued, username) = browser
        .complete_setup(&request.setup_secret, request.username, request.password)
        .await?;
    Ok(signed_in(browser, issued, username))
}

/// Signs the owner in.
pub async fn login(
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
    JsonBody(request): JsonBody<LoginRequest>,
) -> Result<Response, ApiError> {
    let browser = checked(&app, &method, &headers)?;
    let issued = browser
        .login(request.username.clone(), request.password)
        .await?;
    Ok(signed_in(browser, issued, request.username))
}

/// Signs the session out and clears its cookie.
pub async fn logout(
    State(app): State<AppState>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let browser = checked(&app, &method, &headers)?;
    browser.logout(&headers)?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, browser.set_cookie(None));
    Ok(response)
}

/// The server's browser access, if configured, once the request passed
/// its origin checks.
fn checked<'a>(
    app: &'a AppState,
    method: &Method,
    headers: &HeaderMap,
) -> Result<&'a Arc<BrowserAuth>, ApiError> {
    let browser = app
        .browser
        .as_ref()
        .ok_or_else(|| ApiError::new(ErrorCode::NotFound, "browser access is not configured"))?;
    browser.origins.check(method, headers)?;
    Ok(browser)
}

fn signed_in(browser: &BrowserAuth, issued: Issued, username: String) -> Response {
    let status = SessionStatus {
        setup_required: false,
        authenticated: true,
        username: Some(username),
        csrf_token: Some(issued.csrf),
        permissions: Some(OWNER),
    };
    let mut response = Json(status).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, browser.set_cookie(Some(&issued.token)));
    response
}

/// Response middleware: marks a response, error or not, as never to be
/// cached.
pub async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
