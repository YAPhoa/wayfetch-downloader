//! The bundled web UI (architecture brief section 11.2, ADR-32): static
//! files the host hands over, served from the API's own origin.
//!
//! Every file is served below `/assets/<version>/`, where the version is a
//! digest of all the files, so each release's assets can be cached forever
//! without an old page ever mixing them with a new release's. The entry
//! page, `index.html`, is served for `/` and every other browser path
//! outside `/v1/` and `/assets/`, so a reload of any view works; it is
//! revalidated on every load, so an upgrade reaches open browsers on their
//! next reload instead of an old page issuing requests to a new server.
//! The entry page names its assets with the placeholder `{{assets}}`,
//! replaced by `/assets/<version>`.
//!
//! Responses forbid every script, style, and connection beyond this
//! origin, framing, and HTML parsing of strings in scripts (Trusted
//! Types), so text from downloads can only ever be shown as text.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};
use wayfetch_engine::protocol::ErrorCode;

use crate::error::ApiError;

/// The entry page's file name.
pub const INDEX: &str = "index.html";
/// What the entry page writes where its assets' base path goes.
const PLACEHOLDER: &str = "{{assets}}";
/// Where assets are served.
const ASSETS: &str = "/assets";

/// Policies of every UI response. Scripts, styles, images, fonts, and
/// requests stay on this origin; nothing may frame the page; and the DOM
/// refuses to parse strings as HTML.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
    style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'; \
    require-trusted-types-for 'script'; trusted-types 'none'";

/// The UI could not be bundled.
#[derive(Debug, thiserror::Error)]
pub enum UiAssetsError {
    /// There is no entry page.
    #[error("the UI has no {INDEX}")]
    NoIndex,
    /// The entry page is not UTF-8.
    #[error("the UI's {INDEX} is not UTF-8")]
    IndexNotText,
}

/// A UI bundle: files by `/`-separated path, such as `client/feed.js`.
#[derive(Debug, Clone)]
pub struct UiAssets {
    version: String,
    index: Arc<str>,
    files: Arc<HashMap<String, Asset>>,
}

#[derive(Debug)]
struct Asset {
    body: Cow<'static, [u8]>,
    content_type: HeaderValue,
}

impl UiAssets {
    /// Bundles `files`, which must include [`INDEX`].
    pub fn new(
        files: impl IntoIterator<Item = (String, Cow<'static, [u8]>)>,
    ) -> Result<Self, UiAssetsError> {
        let mut files: Vec<_> = files.into_iter().collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut digest = Sha256::new();
        for (path, body) in &files {
            for part in [path.as_bytes(), body] {
                digest.update((part.len() as u64).to_le_bytes());
                digest.update(part);
            }
        }
        let version: String = digest.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let index = files
            .iter()
            .find(|(path, _)| path == INDEX)
            .ok_or(UiAssetsError::NoIndex)?;
        let index = std::str::from_utf8(&index.1)
            .map_err(|_| UiAssetsError::IndexNotText)?
            .replace(PLACEHOLDER, &format!("{ASSETS}/{version}"));
        let files = files
            .into_iter()
            .map(|(path, body)| {
                let content_type = content_type(&path);
                (path, Asset { body, content_type })
            })
            .collect();
        Ok(Self {
            version,
            index: index.into(),
            files: Arc::new(files),
        })
    }

    /// The bundle's version: a digest of every file.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The UI's routes, falling back to the entry page for browser paths.
    pub(crate) fn router<S>(&self) -> Router<S> {
        Router::new()
            .route(&format!("{ASSETS}/{{version}}/{{*path}}"), get(asset))
            .fallback(entry)
            .with_state(self.clone())
    }
}

/// A file's type, from its extension. Text is UTF-8.
fn content_type(path: &str) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let text = mime.type_() == mime_guess::mime::TEXT
        || mime.subtype() == mime_guess::mime::JAVASCRIPT
        || mime.suffix() == Some(mime_guess::mime::XML);
    let value = if text {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };
    HeaderValue::from_str(&value).expect("MIME types are valid header values")
}

/// `GET /assets/{version}/{path}`: a file of this bundle, cacheable for
/// good. Another version's files are gone.
async fn asset(
    State(ui): State<UiAssets>,
    Path((version, path)): Path<(String, String)>,
) -> Response {
    match ui.files.get(&path).filter(|_| version == ui.version) {
        Some(asset) => respond(
            asset.content_type.clone(),
            "public, max-age=31536000, immutable",
            Body::from(asset.body.clone()),
        ),
        None => not_found(),
    }
}

/// Any other path: the entry page for a browser's `GET` outside the API,
/// not found otherwise.
async fn entry(State(ui): State<UiAssets>, method: Method, uri: Uri) -> Response {
    let path = uri.path();
    let browser_path = !(path == "/v1"
        || path.starts_with("/v1/")
        || path == ASSETS
        || path.starts_with("/assets/"));
    if browser_path && (method == Method::GET || method == Method::HEAD) {
        respond(
            HeaderValue::from_static("text/html; charset=utf-8"),
            "no-cache",
            Body::from(ui.index.to_string()),
        )
    } else {
        not_found()
    }
}

fn not_found() -> Response {
    let mut response = ApiError::new(ErrorCode::NotFound, "no such route or file").into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn respond(content_type: HeaderValue, cache: &'static str, body: Body) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    for (name, value) in [
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    fn bundle(index: &str) -> UiAssets {
        UiAssets::new([
            (INDEX.to_owned(), Cow::Owned(index.as_bytes().to_vec())),
            ("main.js".to_owned(), Cow::Borrowed(&b"export {};"[..])),
            ("client/feed.js".to_owned(), Cow::Borrowed(&b"//"[..])),
            ("style.css".to_owned(), Cow::Borrowed(&b"body{}"[..])),
        ])
        .unwrap()
    }

    async fn get(ui: &UiAssets, method: Method, path: &str) -> (StatusCode, Response) {
        let response = ui
            .router::<()>()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        (response.status(), response)
    }

    async fn text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[test]
    fn the_version_changes_with_any_file() {
        let a = bundle("<script src=\"{{assets}}/main.js\">");
        let b = bundle("<script src=\"{{assets}}/main.js\"> ");
        assert_eq!(a.version().len(), 16);
        assert_ne!(a.version(), b.version());
        assert_eq!(
            a.version(),
            bundle("<script src=\"{{assets}}/main.js\">").version()
        );
        assert!(matches!(
            UiAssets::new([("main.js".to_owned(), Cow::Borrowed(&b""[..]))]),
            Err(UiAssetsError::NoIndex)
        ));
    }

    #[tokio::test]
    async fn browser_paths_get_the_revalidated_entry_page() {
        let ui = bundle("<script src=\"{{assets}}/main.js\"></script>");
        for path in ["/", "/jobs/job-1", "/login?next=x"] {
            let (status, response) = get(&ui, Method::GET, path).await;
            assert_eq!(status, StatusCode::OK, "{path}");
            let headers = response.headers();
            assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
            assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
            assert!(
                headers[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("require-trusted-types-for 'script'")
            );
            assert_eq!(
                text(response).await,
                format!("<script src=\"/assets/{}/main.js\"></script>", ui.version())
            );
        }
        let (status, _) = get(&ui, Method::HEAD, "/jobs").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn api_paths_and_other_methods_never_get_the_page() {
        let ui = bundle("x");
        for (method, path) in [
            (Method::GET, "/v1"),
            (Method::GET, "/v1/nope"),
            (Method::GET, "/assets"),
            (Method::GET, "/assets/main.js"),
            (Method::POST, "/"),
            (Method::DELETE, "/jobs"),
        ] {
            let (status, response) = get(&ui, method.clone(), path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert!(text(response).await.contains("not_found"));
        }
    }

    #[tokio::test]
    async fn assets_are_immutable_under_their_version_only() {
        let ui = bundle("x");
        let v = ui.version().to_owned();
        let (status, response) =
            get(&ui, Method::GET, &format!("/assets/{v}/client/feed.js")).await;
        assert_eq!(status, StatusCode::OK);
        let headers = response.headers();
        assert_eq!(
            headers[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(text(response).await, "//");
        let (_, response) = get(&ui, Method::GET, &format!("/assets/{v}/style.css")).await;
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/css; charset=utf-8"
        );
        for path in [
            "/assets/0000000000000000/main.js".to_owned(),
            format!("/assets/{v}/missing.js"),
            format!("/assets/{v}/../main.js"),
        ] {
            let (status, _) = get(&ui, Method::GET, &path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }
}
