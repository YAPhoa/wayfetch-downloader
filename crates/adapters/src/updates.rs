//! The project's releases on GitHub, for update checks: the highest
//! version among the newest published releases, pre-releases included
//! (every release so far is one), compared with the running version.

use std::sync::Arc;

use semver::Version;
use serde::Deserialize;
use url::Url;
use wayfetch_engine::accounts::provider::{ApiClient, ApiMethod, ProviderError};
use wayfetch_engine::protocol::update::UpdateOutcome;
use wayfetch_engine::transfer::writer::BoxFuture;
use wayfetch_engine::updates::UpdateSource;

use crate::common::{Api, Endpoint};

/// The repository's API on GitHub.
pub const OFFICIAL_API: &str = "https://api.github.com/repos/YAPhoa/wayfetch-downloader/";

/// Where every release is listed.
pub const RELEASES_PAGE: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases");

/// Largest answer read: a page of releases with long notes and many files.
const MAX_BODY: usize = 1024 * 1024;

/// The fields of GitHub's release object used here.
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    /// Only the repository's owners see drafts; skipped all the same.
    #[serde(default)]
    draft: bool,
}

/// The newest release on GitHub, through the host's client.
#[derive(Debug)]
pub struct GitHubReleases {
    api: Api,
}

impl GitHubReleases {
    /// The repository's releases at `endpoint`, through `client`.
    pub fn new(client: Arc<dyn ApiClient>, endpoint: Endpoint) -> Self {
        Self {
            api: Api::new(client, "GitHub", endpoint).with_max_body(MAX_BODY),
        }
    }

    async fn outcome(&self, current: &str) -> Result<UpdateOutcome, ProviderError> {
        // The newest 30 releases, newest first. `releases/latest` would
        // leave out pre-releases.
        let url = self.api.endpoint().url("releases");
        let response = self.api.call(ApiMethod::Get, url, None, None).await?;
        match response.status {
            200 => {}
            // No such repository, as for a stand-in with nothing yet.
            404 => return Ok(UpdateOutcome::UpToDate),
            403 | 429 => {
                return Err(ProviderError::Unavailable(
                    "GitHub is limiting requests from this network; try again later".into(),
                ));
            }
            status => {
                return Err(ProviderError::Unavailable(format!(
                    "GitHub answered with HTTP {status}; try again later"
                )));
            }
        }
        let releases: Vec<Release> = self.api.parse(&response, "the releases")?;
        // Tags that are not versions, such as `nightly`, are never offered.
        let Some((newest, release)) = releases
            .into_iter()
            .filter(|release| !release.draft)
            .filter_map(|release| Some((parse_version(&release.tag_name)?, release)))
            .max_by(|(a, _), (b, _)| a.cmp(b))
        else {
            return Ok(UpdateOutcome::UpToDate);
        };
        let running = parse_version(current).ok_or_else(|| {
            ProviderError::Malformed(format!("this version {current} is unreadable"))
        })?;
        if newest <= running {
            return Ok(UpdateOutcome::UpToDate);
        }
        let url = Url::parse(&release.html_url)
            .ok()
            .filter(|url| self.api.endpoint().allows_page(url))
            .ok_or_else(|| self.api.malformed("the newest release: its page"))?;
        Ok(UpdateOutcome::Available {
            version: newest.to_string(),
            url: url.into(),
        })
    }
}

/// A version written as in a tag, with or without a leading `v`.
fn parse_version(text: &str) -> Option<Version> {
    Version::parse(text.strip_prefix('v').unwrap_or(text)).ok()
}

impl UpdateSource for GitHubReleases {
    fn releases(&self) -> &str {
        RELEASES_PAGE
    }

    fn check<'a>(&'a self, current: &'a str) -> BoxFuture<'a, UpdateOutcome> {
        Box::pin(async move {
            self.outcome(current)
                .await
                .unwrap_or_else(|e| UpdateOutcome::Unavailable {
                    problem: e.to_string().into(),
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use wayfetch_engine::accounts::Secret;
    use wayfetch_engine::accounts::provider::{ApiError, ApiErrorKind, ApiRequest, ApiResponse};

    use super::*;

    /// A status and body, or why no answer came.
    type Answer = Result<(u16, &'static str), ApiErrorKind>;

    /// Answers every request with one canned response, and records the
    /// URLs asked for.
    struct Canned {
        answer: Answer,
        asked: Mutex<Vec<String>>,
    }

    impl ApiClient for Canned {
        fn send<'a>(
            &'a self,
            request: &'a ApiRequest,
        ) -> BoxFuture<'a, Result<ApiResponse, ApiError>> {
            self.asked.lock().unwrap().push(request.url.to_string());
            assert_eq!(request.max_body, MAX_BODY);
            let answer = self.answer.map_err(|kind| ApiError {
                kind,
                message: "connection refused".into(),
            });
            Box::pin(async move {
                answer.map(|(status, body)| ApiResponse {
                    status,
                    body: Secret::new(body.as_bytes().to_vec()),
                })
            })
        }
    }

    async fn check(answer: Answer, current: &str) -> UpdateOutcome {
        let client = Arc::new(Canned {
            answer,
            asked: Mutex::default(),
        });
        let releases = GitHubReleases::new(client.clone(), Endpoint::official(OFFICIAL_API));
        let outcome = releases.check(current).await;
        assert_eq!(releases.releases(), RELEASES_PAGE);
        assert_eq!(
            *client.asked.lock().unwrap(),
            ["https://api.github.com/repos/YAPhoa/wayfetch-downloader/releases"]
        );
        outcome
    }

    /// Pre-releases, as every release so far is, newest first, with a
    /// tag that is not a version, and a draft only an owner would see.
    const NEWER: &str = r#"[
        {"tag_name":"nightly","html_url":"https://github.com/YAPhoa/wayfetch-downloader/releases/tag/nightly","prerelease":true},
        {"tag_name":"v9.0.0","html_url":"https://github.com/YAPhoa/wayfetch-downloader/releases/tag/v9.0.0","draft":true},
        {"tag_name":"v0.2.0","html_url":"https://github.com/YAPhoa/wayfetch-downloader/releases/tag/v0.2.0","prerelease":true},
        {"tag_name":"v0.1.1","html_url":"https://github.com/YAPhoa/wayfetch-downloader/releases/tag/v0.1.1","prerelease":true}
    ]"#;

    #[test]
    fn the_api_and_page_belong_to_the_repository() {
        let repository = env!("CARGO_PKG_REPOSITORY");
        let path = repository.strip_prefix("https://github.com/").unwrap();
        assert_eq!(
            OFFICIAL_API,
            format!("https://api.github.com/repos/{path}/")
        );
        assert_eq!(RELEASES_PAGE, format!("{repository}/releases"));
    }

    #[tokio::test]
    async fn the_highest_newer_release_is_offered_with_its_page() {
        assert_eq!(
            check(Ok((200, NEWER)), "0.1.0").await,
            UpdateOutcome::Available {
                version: "0.2.0".into(),
                url: "https://github.com/YAPhoa/wayfetch-downloader/releases/tag/v0.2.0".into(),
            }
        );
    }

    #[tokio::test]
    async fn the_same_or_an_older_release_is_up_to_date() {
        for current in ["0.2.0", "0.10.0", "1.0.0"] {
            assert_eq!(
                check(Ok((200, NEWER)), current).await,
                UpdateOutcome::UpToDate
            );
        }
        // Nothing published yet, or no such repository.
        for answer in [(200, "[]"), (404, r#"{"message":"Not Found"}"#)] {
            assert_eq!(check(Ok(answer), "0.1.0").await, UpdateOutcome::UpToDate);
        }
    }

    #[tokio::test]
    async fn failures_say_why_without_the_address() {
        let problem = |outcome| match outcome {
            UpdateOutcome::Unavailable { problem } => problem.as_str().to_owned(),
            other => panic!("unexpected {other:?}"),
        };
        let cases: [(Answer, &str); 5] = [
            (
                Err(ApiErrorKind::Unreachable),
                "GitHub could not be reached",
            ),
            (Ok((403, "{}")), "limiting requests"),
            (Ok((502, "")), "HTTP 502"),
            (Ok((200, "<html>")), "unexpected answer"),
            (
                Ok((
                    200,
                    r#"[{"tag_name":"v9.0.0","html_url":"http://github.com/x"}]"#,
                )),
                "its page",
            ),
        ];
        for (answer, expected) in cases {
            let text = problem(check(answer, "0.1.0").await);
            assert!(text.contains(expected), "{text:?} lacks {expected:?}");
            assert!(!text.contains("api.github.com"), "{text}");
        }
    }
}
