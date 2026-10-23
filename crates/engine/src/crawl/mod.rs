//! Link discovery for the Links list: finding links in text, and checking
//! each collected link in the background.
//!
//! [`extract`] finds links without the network, [`Inspector`] is the seam to
//! the HTTP client, and [`check`] decides from a few bounded requests
//! whether a link is a file, a page, offline, or unknown. Waiting checks are
//! stored rows, and a link found on a page is never crawled further.

pub mod extract;

use std::time::Duration;

use bytes::Bytes;
use url::Url;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::query::CheckReason;

use crate::transfer::transport::Reach;
use crate::transfer::writer::BoxFuture;

/// Largest page body read to find the links on it.
pub const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;

/// Longest one request of a check may take, body included.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

/// How an [`InspectRequest`] asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectMethod {
    /// `HEAD`.
    Head,
    /// `GET` of the first byte only (`Range: bytes=0-0`), whose body is
    /// never read: what a server that refuses `HEAD` says about a file.
    FirstByte,
    /// `GET` whose body is read, up to [`InspectRequest::max_page`], only
    /// if the answer is a web page (see [`is_page`]); a file's body is
    /// never read.
    Page,
}

/// One metadata request. Redirects are followed by the client within its
/// hop limit and the egress policy; only the answer at the end is
/// reported.
#[derive(Debug, Clone)]
pub struct InspectRequest {
    /// The link, as collected.
    pub url: Url,
    /// How to ask.
    pub method: InspectMethod,
    /// Largest page body read; a longer one fails as too large.
    pub max_page: usize,
    /// Deadline for the whole exchange.
    pub deadline: Duration,
    /// Which addresses it may reach, redirects included.
    pub reach: Reach,
}

/// What a server answered, reduced to what checking needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspection {
    /// The final status.
    pub status: u16,
    /// The URL that answered, after redirects: what relative links on a
    /// page are read against. Never stored, since a redirect target may
    /// carry a signed token.
    pub final_url: Option<Url>,
    /// `Content-Type`, without parameters, lower case.
    pub content_type: Option<String>,
    /// The whole file's length, when the answer says and its body is not
    /// encoded: `Content-Length` of a whole answer, or the total of a
    /// `Content-Range`.
    pub length: Option<u64>,
    /// The file name `Content-Disposition` suggests, if any.
    pub filename: Option<String>,
    /// Whether `Content-Disposition` says `attachment`.
    pub attachment: bool,
    /// A page's body, for [`InspectMethod::Page`] when the answer is one.
    pub page: Option<Bytes>,
    /// Whether it came from an address that is not public, which only the
    /// operator's allowed networks reach.
    pub local: bool,
}

/// Why a request got no answer to classify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectError {
    /// Why, in the terms a check reports.
    pub reason: CheckReason,
    /// A short description for the user; never the URL, which may carry a
    /// token.
    pub message: String,
}

/// Sends the metadata requests of link checks.
pub trait Inspector: Send + Sync + 'static {
    /// Sends `request` once and reads at most its page body.
    fn inspect<'a>(
        &'a self,
        request: &'a InspectRequest,
    ) -> BoxFuture<'a, Result<Inspection, InspectError>>;
}

/// Whether an answer with this type and disposition is a web page rather
/// than a file: HTML that is not offered as an attachment. A download
/// with an `.html` name sent as an attachment is a file.
pub fn is_page(content_type: Option<&str>, attachment: bool) -> bool {
    !attachment && matches!(content_type, Some("text/html" | "application/xhtml+xml"))
}

/// Extensions of web pages rather than files: a link ending in one is not
/// taken for a file without checking.
const PAGE_EXTENSIONS: &[&str] = &[
    "htm", "html", "xhtml", "shtml", "php", "asp", "aspx", "jsp", "jspx", "cgi", "pl", "cfm",
];

/// Whether `url` looks like a file from its last path segment alone: it has
/// an extension of one to eight letters or digits that is not a web page's.
/// Only a hint for what to check; a check decides.
pub fn looks_like_file(url: &Url) -> bool {
    let Some(segment) = url.path_segments().and_then(|mut s| s.next_back()) else {
        return false;
    };
    let Some((stem, extension)) = segment.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && (1..=8).contains(&extension.len())
        && extension.bytes().all(|b| b.is_ascii_alphanumeric())
        && !PAGE_EXTENSIONS
            .iter()
            .any(|page| extension.eq_ignore_ascii_case(page))
}

/// What checking a link found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// It serves a file.
    File {
        /// The name its server suggested, if any.
        name: Option<String>,
        /// Its size, when the server said.
        size: Option<u64>,
    },
    /// It is a web page. `links` holds the `http(s)` links on it, in
    /// order and each once, when it was read; `None` when it was not.
    Page {
        /// The links on it, if read.
        links: Option<Vec<Url>>,
        /// Whether it was served from an address that is not public. Only
        /// such a page, one on a local server the operator allows, may
        /// lead requests into local networks: links found on any other
        /// page reach public addresses only.
        local: bool,
    },
    /// Its server says it is gone.
    Offline(CheckReason, String),
    /// Checking could not tell.
    Unknown(CheckReason, String),
}

/// Who answered a link's check in place of its own server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Checker {
    /// The provider of an account that would download the link.
    Account(ProviderId),
    /// The file host the link is on, which downloads it without an account.
    Host(ProviderId),
}

impl Checker {
    /// The service that answered.
    pub(crate) fn service(&self) -> &ProviderId {
        match self {
            Self::Account(service) | Self::Host(service) => service,
        }
    }
}

/// What an answer says about the link, without its page's links.
fn classify(answer: &Inspection) -> Checked {
    let status = answer.status;
    let http = || format!("The server answered HTTP {status}.");
    match status {
        200..=299 | 416 => {
            if is_page(answer.content_type.as_deref(), answer.attachment) {
                Checked::Page {
                    links: None,
                    local: answer.local,
                }
            } else if answer.length.is_some_and(|n| i64::try_from(n).is_err()) {
                // Larger than any file the store can record, or a
                // filesystem can hold: the answer cannot be trusted.
                Checked::Unknown(
                    CheckReason::Unreadable,
                    "The server reported a size no file can have.".into(),
                )
            } else {
                Checked::File {
                    name: answer.filename.clone(),
                    size: answer.length,
                }
            }
        }
        404 | 410 => Checked::Offline(CheckReason::NotFound, http()),
        401 | 403 | 407 => Checked::Unknown(CheckReason::AccessDenied, http()),
        429 => Checked::Unknown(CheckReason::RateLimited, http()),
        408 => Checked::Unknown(CheckReason::TimedOut, http()),
        500..=599 => Checked::Unknown(CheckReason::ServerError, http()),
        _ => Checked::Unknown(CheckReason::Unreadable, http()),
    }
}

/// Whether an answer to `HEAD` is worth trusting: a success. A server may
/// refuse or mishandle `HEAD` while serving `GET`, so anything else is
/// asked again with a first-byte `GET`.
fn head_usable(answer: &Inspection) -> bool {
    (200..300).contains(&answer.status)
}

/// Checks `url`, reaching only what `reach` permits. With `analyze`, a
/// link that turns out to be a web page is read, and the links on it are
/// returned; a link that does not look like a file is then asked for as a
/// page straight away, saving a `HEAD`.
pub async fn check(inspector: &dyn Inspector, url: &Url, reach: Reach, analyze: bool) -> Checked {
    let ask = |method| InspectRequest {
        url: url.clone(),
        method,
        max_page: MAX_PAGE_BYTES,
        deadline: REQUEST_DEADLINE,
        reach,
    };
    let failed = |e: InspectError| match e.reason {
        CheckReason::NotFound => Checked::Offline(e.reason, e.message),
        _ => Checked::Unknown(e.reason, e.message),
    };
    let first = if analyze && !looks_like_file(url) {
        ask(InspectMethod::Page)
    } else {
        ask(InspectMethod::Head)
    };
    let mut answer = match inspector.inspect(&first).await {
        Ok(answer) => answer,
        Err(e) => return failed(e),
    };
    if first.method == InspectMethod::Head && !head_usable(&answer) {
        answer = match inspector.inspect(&ask(InspectMethod::FirstByte)).await {
            Ok(answer) => answer,
            Err(e) => return failed(e),
        };
    }
    let checked = classify(&answer);
    if !(analyze && matches!(checked, Checked::Page { .. })) {
        return checked;
    }
    if answer.page.is_none() {
        answer = match inspector.inspect(&ask(InspectMethod::Page)).await {
            Ok(answer) => answer,
            Err(e) => return failed(e),
        };
        // The page may have changed into something else meanwhile.
        let again = classify(&answer);
        if !matches!(again, Checked::Page { .. }) {
            return again;
        }
    }
    let Some(body) = answer.page.take() else {
        return Checked::Unknown(
            CheckReason::Unreadable,
            "The page's content could not be read.".into(),
        );
    };
    let base = answer.final_url.clone().unwrap_or_else(|| url.clone());
    // Parsing a page is CPU work; keep it off the runtime's workers.
    let links = tokio::task::spawn_blocking(move || {
        extract::page_links(&String::from_utf8_lossy(&body), &base)
    })
    .await
    .unwrap_or_default();
    Checked::Page {
        links: Some(links),
        local: answer.local,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// Answers requests from a script, recording each.
    struct Script {
        answers: Mutex<VecDeque<Result<Inspection, InspectError>>>,
        asked: Mutex<Vec<InspectMethod>>,
    }

    impl Script {
        fn new(answers: Vec<Result<Inspection, InspectError>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<InspectMethod> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Inspector for Script {
        fn inspect<'a>(
            &'a self,
            request: &'a InspectRequest,
        ) -> BoxFuture<'a, Result<Inspection, InspectError>> {
            self.asked.lock().unwrap().push(request.method);
            let answer = self.answers.lock().unwrap().pop_front().expect("scripted");
            Box::pin(async move { answer })
        }
    }

    fn answer(status: u16, content_type: &str) -> Inspection {
        Inspection {
            status,
            content_type: Some(content_type.into()),
            ..Inspection::default()
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn file_looking_links_are_told_from_pages() {
        for file in [
            "https://a.example/f.zip",
            "https://a.example/d/Movie.MKV?sig=1",
            "https://a.example/x.tar.gz",
        ] {
            assert!(looks_like_file(&url(file)), "{file}");
        }
        for other in [
            "https://a.example/",
            "https://a.example/download",
            "https://a.example/index.HTML",
            "https://a.example/view.php?id=1",
            "https://a.example/.hidden",
            "https://a.example/a.verylongext",
            "https://a.example/a.b-c",
        ] {
            assert!(!looks_like_file(&url(other)), "{other}");
        }
    }

    #[tokio::test]
    async fn a_file_is_named_and_sized_by_its_server() {
        let script = Script::new(vec![Ok(Inspection {
            status: 200,
            content_type: Some("application/zip".into()),
            length: Some(12),
            filename: Some("Real name.zip".into()),
            attachment: true,
            ..Inspection::default()
        })]);
        let checked = check(
            &script,
            &url("https://a.example/f.zip"),
            Reach::Allowed,
            true,
        )
        .await;
        assert_eq!(
            checked,
            Checked::File {
                name: Some("Real name.zip".into()),
                size: Some(12)
            }
        );
        assert_eq!(script.asked(), [InspectMethod::Head]);
    }

    #[tokio::test]
    async fn a_refused_head_is_asked_again_with_one_byte() {
        // A valid empty file answers a first-byte request with 416.
        let empty = Inspection {
            status: 416,
            length: Some(0),
            ..Inspection::default()
        };
        let script = Script::new(vec![Ok(answer(405, "text/plain")), Ok(empty)]);
        let checked = check(
            &script,
            &url("https://a.example/empty.bin"),
            Reach::Allowed,
            false,
        )
        .await;
        assert_eq!(
            checked,
            Checked::File {
                name: None,
                size: Some(0)
            }
        );
        assert_eq!(
            script.asked(),
            [InspectMethod::Head, InspectMethod::FirstByte]
        );
    }

    #[tokio::test]
    async fn a_login_page_behind_a_file_name_is_a_page_not_a_file() {
        let script = Script::new(vec![Ok(answer(200, "text/html"))]);
        let checked = check(
            &script,
            &url("https://a.example/f.zip"),
            Reach::Allowed,
            false,
        )
        .await;
        assert_eq!(
            checked,
            Checked::Page {
                links: None,
                local: false
            }
        );
        // An attachment named .html is a file.
        let script = Script::new(vec![Ok(Inspection {
            attachment: true,
            ..answer(200, "text/html")
        })]);
        let checked = check(
            &script,
            &url("https://a.example/save"),
            Reach::Allowed,
            false,
        )
        .await;
        assert!(matches!(checked, Checked::File { .. }), "{checked:?}");
    }

    #[tokio::test]
    async fn an_analyzed_page_lists_its_links_read_against_where_it_ended() {
        let page = Inspection {
            final_url: Some(url("https://b.example/dir/")),
            page: Some(Bytes::from_static(
                b"<a href=f.zip>f</a><a href='#top'>top</a><a href=/g.bin>g</a>",
            )),
            ..answer(200, "text/html")
        };
        let script = Script::new(vec![Ok(page)]);
        let checked = check(
            &script,
            &url("https://a.example/release"),
            Reach::Allowed,
            true,
        )
        .await;
        assert_eq!(
            checked,
            Checked::Page {
                links: Some(vec![
                    url("https://b.example/dir/f.zip"),
                    url("https://b.example/g.bin")
                ]),
                local: false
            }
        );
        assert_eq!(script.asked(), [InspectMethod::Page]);
    }

    #[tokio::test]
    async fn a_file_looking_link_that_is_a_page_is_read_only_when_analyzed() {
        let page = Inspection {
            page: Some(Bytes::from_static(b"<a href=f.zip>f</a>")),
            ..answer(200, "text/html")
        };
        let script = Script::new(vec![Ok(answer(200, "text/html")), Ok(page)]);
        let checked = check(
            &script,
            &url("https://a.example/a/f.rar"),
            Reach::Allowed,
            true,
        )
        .await;
        assert_eq!(
            checked,
            Checked::Page {
                links: Some(vec![url("https://a.example/a/f.zip")]),
                local: false
            }
        );
        assert_eq!(script.asked(), [InspectMethod::Head, InspectMethod::Page]);
    }

    #[tokio::test]
    async fn only_a_definite_absence_is_offline() {
        let cases = [
            (404, CheckReason::NotFound, true),
            (410, CheckReason::NotFound, true),
            (403, CheckReason::AccessDenied, false),
            (429, CheckReason::RateLimited, false),
            (503, CheckReason::ServerError, false),
            (418, CheckReason::Unreadable, false),
        ];
        for (status, reason, offline) in cases {
            let script = Script::new(vec![
                Ok(answer(status, "text/html")),
                Ok(answer(status, "text/html")),
            ]);
            let checked = check(
                &script,
                &url("https://a.example/f.zip"),
                Reach::Allowed,
                false,
            )
            .await;
            let message = format!("The server answered HTTP {status}.");
            let expected = if offline {
                Checked::Offline(reason, message)
            } else {
                Checked::Unknown(reason, message)
            };
            assert_eq!(checked, expected);
        }
        let script = Script::new(vec![Err(InspectError {
            reason: CheckReason::TimedOut,
            message: "no answer in time".into(),
        })]);
        let checked = check(
            &script,
            &url("https://a.example/f.zip"),
            Reach::Allowed,
            false,
        )
        .await;
        assert_eq!(
            checked,
            Checked::Unknown(CheckReason::TimedOut, "no answer in time".into())
        );
    }

    #[tokio::test]
    async fn a_size_no_file_can_have_is_not_believed() {
        let script = Script::new(vec![Ok(Inspection {
            length: Some(1 << 63),
            ..answer(200, "application/zip")
        })]);
        let checked = check(
            &script,
            &url("https://a.example/f.zip"),
            Reach::Allowed,
            false,
        )
        .await;
        assert_eq!(
            checked,
            Checked::Unknown(
                CheckReason::Unreadable,
                "The server reported a size no file can have.".into()
            )
        );
    }
}
