//! YouTube: recognizing video links without the network, and reading them
//! through [`crate::ytdlp`].
//!
//! yt-dlp is only given the canonical watch page for the video ID, so
//! playlist or tracking parameters never change what it reads.

use std::sync::Arc;

use url::Url;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::domain::name::ProviderId;
use wayfetch_engine::media::{MediaError, MediaInfo, MediaPlan, MediaService};
use wayfetch_engine::protocol::media::{MediaChoice, PluginSummary};
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::ytdlp::YtDlp;

/// The service's ID, as in the UI's catalogue.
pub const SERVICE: &str = "youtube";

/// A YouTube video ID: 11 characters of the URL-safe base64 alphabet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VideoId(String);

impl VideoId {
    /// `id` if it has a video ID's shape.
    pub fn new(id: &str) -> Option<Self> {
        (id.len() == 11
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        .then(|| Self(id.to_owned()))
    }

    /// The ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The video's canonical watch page.
    pub fn watch_url(&self) -> Url {
        let mut url = Url::parse("https://www.youtube.com/watch").expect("static URL");
        url.query_pairs_mut().append_pair("v", &self.0);
        url
    }
}

/// The video `link` names, if it is a YouTube video link: a watch page,
/// a short, a live stream's page, an embed, or a `youtu.be` short link,
/// on YouTube's own hosts only, without credentials or a custom port.
/// Playlists and channels are not videos; a watch page that also names a
/// playlist still names its video.
pub fn video_id(link: &Url) -> Option<VideoId> {
    if !matches!(link.scheme(), "http" | "https")
        || link.port().is_some()
        || !link.username().is_empty()
        || link.password().is_some()
    {
        return None;
    }
    let host = link.host_str()?;
    let host = host.strip_suffix('.').unwrap_or(host);
    let mut segments = link.path_segments()?.filter(|s| !s.is_empty());
    let id = match host {
        "youtu.be" => segments.next()?.to_owned(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            match segments.next()? {
                "watch" => link
                    .query_pairs()
                    .find(|(key, _)| key == "v")
                    .map(|(_, v)| v.into_owned())?,
                "shorts" | "live" | "embed" | "v" => segments.next()?.to_owned(),
                _ => return None,
            }
        }
        "youtube-nocookie.com" | "www.youtube-nocookie.com" => match segments.next()? {
            "embed" => segments.next()?.to_owned(),
            _ => return None,
        },
        _ => return None,
    };
    VideoId::new(&id)
}

/// YouTube videos, read through yt-dlp.
pub struct YouTube {
    id: ProviderId,
    ytdlp: Arc<YtDlp>,
}

impl YouTube {
    /// The service, reading videos with `ytdlp`.
    pub fn new(ytdlp: Arc<YtDlp>) -> Self {
        Self {
            id: ProviderId::new(SERVICE).expect("a valid service ID"),
            ytdlp,
        }
    }

    /// The canonical page of the video `link` names.
    fn page(link: &Url) -> Result<Url, MediaError> {
        video_id(link).map(|id| id.watch_url()).ok_or_else(|| {
            MediaError::new(
                FailureCode::UnsupportedCapability,
                "not a YouTube video link",
            )
        })
    }
}

impl MediaService for YouTube {
    fn service(&self) -> &ProviderId {
        &self.id
    }

    fn recognizes(&self, link: &Url) -> bool {
        video_id(link).is_some()
    }

    fn plugins(&self) -> Vec<PluginSummary> {
        vec![self.ytdlp.plugin().summary(vec![self.id.clone()])]
    }

    fn probe<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, Result<MediaInfo, MediaError>> {
        Box::pin(async move { self.ytdlp.describe(&Self::page(link)?).await?.info() })
    }

    fn resolve<'a>(
        &'a self,
        link: &'a Url,
        choice: MediaChoice,
        converter: bool,
    ) -> BoxFuture<'a, Result<MediaPlan, MediaError>> {
        Box::pin(async move {
            self.ytdlp
                .describe(&Self::page(link)?)
                .await?
                .plan(choice, converter)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(link: &str) -> Option<String> {
        video_id(&Url::parse(link).unwrap()).map(|id| id.0)
    }

    #[test]
    fn video_links_name_their_video() {
        for link in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?v=dQw4w9WgXcQ&t=42s",
            "https://www.youtube.com/watch?list=PL123&v=dQw4w9WgXcQ&index=2",
            "http://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&feature=share",
            "https://WWW.YouTube.com./watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc&t=10",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ?feature=share",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/watch?v=a_b-c_d-e_f",
        ] {
            assert!(id(link).is_some(), "{link}");
        }
        assert_eq!(
            id("https://youtu.be/dQw4w9WgXcQ").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn other_links_are_not_videos() {
        for link in [
            "https://www.youtube.com/",
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/@channel",
            "https://www.youtube.com/channel/UC123",
            "https://www.youtube.com/results?search_query=x",
            "https://www.youtube.com/watch",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQx",
            "https://www.youtube.com/watch?v=dQw4w9WgX%21Q",
            "https://youtu.be/",
            "https://notyoutube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com.evil.test/watch?v=dQw4w9WgXcQ",
            "https://evil.test/?next=https://youtu.be/dQw4w9WgXcQ",
            "https://user@www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com:8443/watch?v=dQw4w9WgXcQ",
            "ftp://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://gaming.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube-nocookie.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(id(link), None, "{link}");
        }
    }

    #[test]
    fn the_watch_url_is_canonical() {
        let id = VideoId::new("dQw4w9WgXcQ").unwrap();
        assert_eq!(
            id.watch_url().as_str(),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
    }
}
