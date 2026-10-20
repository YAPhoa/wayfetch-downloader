//! yt-dlp as a media resolver (ADR-27): it reads a video page's title and
//! formats, and the stream links for a choice; the engine downloads the
//! streams itself.
//!
//! yt-dlp runs once per check and once per download start, with
//! `--ignore-config` so no configuration file on the host changes what it
//! does, `-J` so it only describes the video, and `--no-playlist`. Only
//! formats served whole over HTTP(S) with an exact size and no DRM are
//! used; fragmented formats (DASH, HLS) are not offered yet.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use url::Url;
use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::media::{
    Container, MAX_IDENTITY_BYTES, MediaError, MediaInfo, MediaPlan, MediaStream, Recipe,
};
use wayfetch_engine::protocol::media::{AudioFormat, MediaChoice, PluginRole};

use crate::plugin::{Check, Describe, Limits, Plugin, Program, RunError, last_line};

/// The plugin.
pub const PLUGIN: Describe = Describe {
    id: "yt-dlp",
    name: "yt-dlp",
    role: PluginRole::MediaResolver,
};

/// Longest one run may take; the engine's own deadline is longer.
const DEADLINE: Duration = Duration::from_secs(80);

/// The most a video's description may be. Captions and thumbnails make
/// a long video's answer a few MiB.
const MAX_ANSWER_BYTES: usize = 32 * 1024 * 1024;

/// yt-dlp and how to run it.
#[derive(Debug, Clone)]
pub struct YtDlp {
    plugin: Arc<Plugin>,
    cache_dir: Option<PathBuf>,
    js_runtimes: Option<String>,
}

impl YtDlp {
    /// How yt-dlp is checked. It keeps its cache (player code it has read)
    /// in `cache_dir`, if any, and Deno, which reads YouTube's player code
    /// for it, keeps its own cache beside it rather than in a home
    /// directory that may be read-only.
    pub fn check(cache_dir: Option<&Path>) -> Check {
        Check {
            args: &["--ignore-config", "--version"],
            version: |out| out.lines().next().map(|v| v.trim().to_owned()),
            env: cache_dir
                .map(|dir| ("DENO_DIR", dir.join("deno").into_os_string()))
                .into_iter()
                .collect(),
            confinement: None,
        }
    }

    /// yt-dlp as `plugin`, keeping its cache in `cache_dir`, or none;
    /// `js_runtimes` is passed to its `--js-runtimes` option if given.
    pub fn new(
        plugin: Arc<Plugin>,
        cache_dir: Option<PathBuf>,
        js_runtimes: Option<String>,
    ) -> Self {
        Self {
            plugin,
            cache_dir,
            js_runtimes,
        }
    }

    /// yt-dlp at `path`, if found, checked by asking its version, as a
    /// plugin Settings cannot update.
    pub async fn detect(
        path: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
        js_runtimes: Option<String>,
    ) -> Self {
        let check = Self::check(cache_dir.as_deref());
        let program = Program::detect(PLUGIN, path, &check).await;
        let plugin = Arc::new(Plugin::new(PLUGIN, check, program, false));
        Self::new(plugin, cache_dir, js_runtimes)
    }

    /// The plugin.
    pub fn plugin(&self) -> &Arc<Plugin> {
        &self.plugin
    }

    /// The video `page` shows, described.
    pub async fn describe(&self, page: &Url) -> Result<Video, MediaError> {
        let mut args: Vec<String> = [
            "--ignore-config",
            "--no-playlist",
            "--no-warnings",
            "--no-progress",
            "--socket-timeout",
            "20",
            "-J",
        ]
        .map(str::to_owned)
        .into();
        match &self.cache_dir {
            Some(dir) => args.extend(["--cache-dir".into(), dir.display().to_string()]),
            None => args.push("--no-cache-dir".into()),
        }
        if let Some(runtimes) = &self.js_runtimes {
            args.extend(["--js-runtimes".into(), runtimes.clone()]);
        }
        args.extend(["--".into(), page.to_string()]);
        let limits = Limits {
            deadline: DEADLINE,
            stdout_bytes: MAX_ANSWER_BYTES,
        };
        let output = self
            .plugin
            .program()
            .run(&args, limits)
            .await
            .map_err(failure)?;
        serde_json::from_slice(&output.stdout).map_err(|_| {
            MediaError::new(
                FailureCode::AdapterParseFailure,
                "yt-dlp's description of the video could not be read",
            )
        })
    }
}

/// What a failed run means for the person.
fn failure(err: RunError) -> MediaError {
    let RunError::Failed { stderr, .. } = &err else {
        return MediaError::new(FailureCode::UnsupportedCapability, format!("yt-dlp {err}"));
    };
    let message = stderr
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix("ERROR:"))
        .map_or_else(|| last_line(stderr), last_line);
    // `[youtube] ID: why` names the extractor and video first.
    let message = match message.strip_prefix('[').and_then(|m| m.split_once("] ")) {
        Some((_, rest)) => rest
            .split_once(": ")
            .map_or(rest, |(_, why)| why)
            .to_owned(),
        None => message,
    };
    let lower = message.to_lowercase();
    let code = if [
        "unavailable",
        "private video",
        "removed",
        "terminated",
        "does not exist",
    ]
    .iter()
    .any(|w| lower.contains(w))
    {
        FailureCode::ResourceDeleted
    } else if ["sign in", "confirm your age", "not a bot"]
        .iter()
        .any(|w| lower.contains(w))
    {
        FailureCode::ChallengeRequired
    } else if [
        "timed out",
        "unable to download",
        "connection",
        "http error 5",
    ]
    .iter()
    .any(|w| lower.contains(w))
    {
        FailureCode::RequestTimeout
    } else {
        FailureCode::AdapterParseFailure
    };
    MediaError::new(code, message)
}

/// A video as yt-dlp describes it; only what Wayfetch reads.
#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<serde_json::Number>,
    #[serde(default)]
    live_status: Option<String>,
    #[serde(default, rename = "_type")]
    kind: Option<String>,
    #[serde(default)]
    formats: Vec<Format>,
}

/// One format yt-dlp found.
#[derive(Debug, Clone, Deserialize)]
struct Format {
    format_id: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    ext: Option<String>,
    #[serde(default)]
    vcodec: Option<String>,
    #[serde(default)]
    acodec: Option<String>,
    #[serde(default)]
    height: Option<serde_json::Number>,
    #[serde(default)]
    filesize: Option<serde_json::Number>,
    #[serde(default)]
    tbr: Option<serde_json::Number>,
    #[serde(default)]
    has_drm: serde_json::Value,
    #[serde(default)]
    http_headers: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    downloader_options: Option<DownloaderOptions>,
}

#[derive(Debug, Clone, Deserialize)]
struct DownloaderOptions {
    #[serde(default)]
    http_chunk_size: Option<serde_json::Number>,
}

/// A format Wayfetch can download.
#[derive(Debug, Clone)]
struct Usable<'a> {
    format: &'a Format,
    url: Url,
    length: u64,
    video: bool,
    audio: bool,
    height: Option<u32>,
    ext: &'a str,
    rate: f64,
}

impl Usable<'_> {
    fn stream(&self) -> MediaStream {
        let header = |name: &str| {
            self.format
                .http_headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .and_then(|(_, v)| v.as_str())
                .map(str::to_owned)
        };
        MediaStream {
            url: self.url.clone(),
            length: self.length,
            user_agent: header("User-Agent"),
            chunk_bytes: self
                .format
                .downloader_options
                .as_ref()
                .and_then(|o| o.http_chunk_size.as_ref()?.as_u64())
                .filter(|&n| n > 0),
        }
    }

    /// Whether this is sound alone.
    fn sound(&self) -> bool {
        self.audio && !self.video
    }
}

impl Format {
    fn usable(&self) -> Option<Usable<'_>> {
        let codec = |c: &Option<String>| match c.as_deref() {
            Some("none") => Some(false),
            Some(_) => Some(true),
            // Unknown codecs are not guessed.
            None => None,
        };
        let (video, audio) = (codec(&self.vcodec)?, codec(&self.acodec)?);
        let url = Url::parse(self.url.as_deref()?).ok()?;
        let whole = matches!(self.protocol.as_deref(), Some("https" | "http"))
            && matches!(url.scheme(), "https" | "http");
        let drm = !matches!(
            self.has_drm,
            serde_json::Value::Null | serde_json::Value::Bool(false)
        );
        let length = self.filesize.as_ref()?.as_u64().filter(|&n| n > 0)?;
        let height = self
            .height
            .as_ref()
            .and_then(serde_json::Number::as_u64)
            .and_then(|h| u32::try_from(h).ok())
            .filter(|&h| h > 0);
        if !whole || drm || !(video || audio) || (video && height.is_none()) {
            return None;
        }
        Some(Usable {
            format: self,
            url,
            length,
            video,
            audio,
            height,
            ext: self.ext.as_deref().unwrap_or(""),
            rate: self
                .tbr
                .as_ref()
                .and_then(serde_json::Number::as_f64)
                .unwrap_or(0.0),
        })
    }
}

/// The best of `formats` by bit rate, then size.
fn best<'a, 'b>(formats: impl Iterator<Item = &'b Usable<'a>>) -> Option<&'b Usable<'a>>
where
    'a: 'b,
{
    formats.max_by(|a, b| a.rate.total_cmp(&b.rate).then(a.length.cmp(&b.length)))
}

impl Video {
    fn usable(&self) -> Result<Vec<Usable<'_>>, MediaError> {
        if self.kind.as_deref().is_some_and(|k| k != "video") {
            return Err(MediaError::new(
                FailureCode::UnsupportedCapability,
                "This link is a playlist or channel, not one video.",
            ));
        }
        if matches!(
            self.live_status.as_deref(),
            Some("is_live" | "is_upcoming" | "post_live")
        ) {
            return Err(MediaError::new(
                FailureCode::UnsupportedCapability,
                "Live streams cannot be downloaded yet.",
            ));
        }
        let usable: Vec<_> = self.formats.iter().filter_map(Format::usable).collect();
        if usable.is_empty() {
            return Err(MediaError::new(
                FailureCode::UnsupportedCapability,
                "Wayfetch found no format of this video it can download.",
            ));
        }
        Ok(usable)
    }

    fn title(&self) -> String {
        self.title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| self.id.clone())
    }

    /// The title and formats, as a check shows them.
    pub fn info(&self) -> Result<MediaInfo, MediaError> {
        let usable = self.usable()?;
        let heights = |progressive: bool| {
            usable
                .iter()
                .filter(|u| u.video && (!progressive || u.audio))
                .filter_map(|u| u.height)
                .collect()
        };
        Ok(MediaInfo {
            title: self.title(),
            duration_seconds: self
                .duration
                .as_ref()
                .and_then(serde_json::Number::as_f64)
                .filter(|d| d.is_finite() && *d >= 0.0)
                // Whole seconds; a fraction of one is not worth showing.
                .map(|d| d.round() as u64),
            heights: heights(false),
            progressive: heights(true),
            audio: usable.iter().any(Usable::sound),
            m4a: usable.iter().any(|u| u.sound() && u.ext == "m4a"),
        }
        .normalized())
    }

    /// The streams that make `choice`, joined or converted only if
    /// `converter`.
    pub fn plan(&self, choice: MediaChoice, converter: bool) -> Result<MediaPlan, MediaError> {
        let info = self.info()?;
        let usable = self.usable()?;
        let unavailable = || {
            MediaError::new(
                FailureCode::UnsupportedCapability,
                "This video does not offer that format any more.",
            )
        };
        if !info.can_make(choice, converter) {
            return Err(unavailable());
        }
        let sound = || usable.iter().filter(|u| u.sound());
        let (streams, recipe, extension): (Vec<&Usable<'_>>, _, _) = match choice {
            MediaChoice::Audio {
                format: AudioFormat::Mp3,
            } => (
                vec![best(sound()).ok_or_else(unavailable)?],
                Recipe::Mp3,
                "mp3",
            ),
            MediaChoice::Audio {
                format: AudioFormat::M4a,
            } => (
                vec![best(sound().filter(|u| u.ext == "m4a")).ok_or_else(unavailable)?],
                Recipe::Copy,
                "m4a",
            ),
            MediaChoice::Video { max_height } => {
                let height = info
                    .height_for(max_height, converter)
                    .ok_or_else(unavailable)?;
                let at = |u: &&Usable<'_>| u.height == Some(height);
                let pictures: Vec<_> = usable
                    .iter()
                    .filter(|u| u.video && !u.audio)
                    .filter(at)
                    .collect();
                let whole = best(usable.iter().filter(|u| u.video && u.audio).filter(at));
                match (converter, pictures.is_empty(), whole) {
                    (true, false, _) => join(&pictures, &usable).ok_or_else(unavailable)?,
                    (_, _, Some(whole)) => (vec![whole], Recipe::Copy, extension_of(whole)),
                    _ => return Err(unavailable()),
                }
            }
        };
        let identity = format!(
            "{};{};{}",
            PLUGIN.id,
            self.id,
            streams
                .iter()
                .map(|u| u.format.format_id.as_str())
                .collect::<Vec<_>>()
                .join("+")
        );
        if identity.len() > MAX_IDENTITY_BYTES {
            return Err(MediaError::new(
                FailureCode::AdapterParseFailure,
                "yt-dlp named the video's formats too long to record",
            ));
        }
        Ok(MediaPlan {
            identity,
            title: info.title,
            extension: extension.into(),
            streams: streams.iter().map(|u| u.stream()).collect(),
            recipe,
        })
    }
}

/// A file name extension a single stream is saved with.
fn extension_of(u: &Usable<'_>) -> &'static str {
    match u.ext {
        "webm" => "webm",
        "m4a" => "m4a",
        "mp3" => "mp3",
        _ => "mp4",
    }
}

/// A picture of `pictures` and the sound to join it with: MP4 with M4A
/// sound where there is both, as most players play it; else WebM with WebM
/// sound; else the best of each in Matroska.
fn join<'a, 'b>(
    pictures: &[&'b Usable<'a>],
    usable: &'b [Usable<'a>],
) -> Option<(Vec<&'b Usable<'a>>, Recipe, &'static str)> {
    let sound = |ext: Option<&str>| {
        best(
            usable
                .iter()
                .filter(|u| u.sound() && ext.is_none_or(|e| u.ext == e)),
        )
    };
    let picture = |ext: &str| best(pictures.iter().copied().filter(|u| u.ext == ext));
    for (picture_ext, sound_ext, container, extension) in [
        ("mp4", "m4a", Container::Mp4, "mp4"),
        ("webm", "webm", Container::Webm, "webm"),
    ] {
        if let (Some(p), Some(s)) = (picture(picture_ext), sound(Some(sound_ext))) {
            return Some((vec![p, s], Recipe::Join(container), extension));
        }
    }
    let p = best(pictures.iter().copied())?;
    Some((vec![p, sound(None)?], Recipe::Join(Container::Mkv), "mkv"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn format(
        id: &str,
        ext: &str,
        vcodec: &str,
        acodec: &str,
        height: Option<u32>,
        tbr: f64,
    ) -> serde_json::Value {
        json!({
            "format_id": id,
            "url": format!("https://cdn.synthetic.example/{id}?sig=SECRET"),
            "protocol": "https",
            "ext": ext,
            "vcodec": vcodec,
            "acodec": acodec,
            "height": height,
            "filesize": 1000 + id.len(),
            "tbr": tbr,
            "http_headers": {"User-Agent": "synthetic-agent", "Accept": "*/*"},
            "downloader_options": {"http_chunk_size": 10_485_760},
        })
    }

    /// A synthetic description in yt-dlp's shape: progressive 360p, MP4 and
    /// WebM pictures, M4A and WebM sound, and formats Wayfetch skips.
    fn video() -> Video {
        let mut formats = vec![
            format("18", "mp4", "avc1.42001E", "mp4a.40.2", Some(360), 500.0),
            format("137", "mp4", "avc1.640028", "none", Some(1080), 4000.0),
            format("248", "webm", "vp9", "none", Some(1080), 3000.0),
            format("136", "mp4", "avc1.4d401f", "none", Some(720), 2000.0),
            format("302", "webm", "vp9", "none", Some(720), 2500.0),
            format("140", "m4a", "none", "mp4a.40.2", None, 128.0),
            format("251", "webm", "none", "opus", None, 160.0),
        ];
        // Fragmented, sizeless, DRM, and storyboard formats are skipped.
        let mut hls = format("95", "mp4", "avc1", "mp4a", Some(2160), 9000.0);
        hls["protocol"] = json!("m3u8_native");
        let mut sizeless = format("401", "mp4", "av01", "none", Some(2160), 9000.0);
        sizeless["filesize"] = json!(null);
        let mut drm = format("999", "mp4", "avc1", "none", Some(1440), 9000.0);
        drm["has_drm"] = json!("maybe");
        let storyboard = format("sb0", "mhtml", "none", "none", None, 0.0);
        formats.extend([hls, sizeless, drm, storyboard]);
        serde_json::from_value(json!({
            "id": "abcdefghijk",
            "title": "Synthetic: video",
            "duration": 61.6,
            "live_status": "not_live",
            "_type": "video",
            "thumbnails": [{"url": "https://i.synthetic.example/x.jpg"}],
            "formats": formats,
        }))
        .unwrap()
    }

    fn ids(plan: &MediaPlan) -> &str {
        plan.identity.rsplit(';').next().unwrap()
    }

    #[test]
    fn the_check_lists_heights_and_sound_of_whole_formats_only() {
        let info = video().info().unwrap();
        assert_eq!(info.title, "Synthetic: video");
        assert_eq!(info.duration_seconds, Some(62));
        assert_eq!(info.heights, [1080, 720, 360]);
        assert_eq!(info.progressive, [360]);
        assert!(info.audio && info.m4a);
    }

    #[test]
    fn video_choices_prefer_mp4_with_m4a_and_fall_back_to_one_stream() {
        let video = video();
        let best = video.plan(MediaChoice::default(), true).unwrap();
        assert_eq!(ids(&best), "137+140");
        assert_eq!(best.recipe, Recipe::Join(Container::Mp4));
        assert_eq!(best.extension, "mp4");
        assert_eq!(best.identity, "yt-dlp;abcdefghijk;137+140");
        let stream = &best.streams[0];
        assert_eq!(stream.user_agent.as_deref(), Some("synthetic-agent"));
        assert_eq!(stream.chunk_bytes, Some(10_485_760));
        assert_eq!(stream.length, 1003);
        assert_eq!(best.checked_length().unwrap(), 1003 + 1003);

        let limited = video
            .plan(
                MediaChoice::Video {
                    max_height: Some(1000),
                },
                true,
            )
            .unwrap();
        assert_eq!(ids(&limited), "136+140");
        // Without a converter, the best is the one stream with sound.
        let single = video.plan(MediaChoice::default(), false).unwrap();
        assert_eq!(ids(&single), "18");
        assert_eq!(single.recipe, Recipe::Copy);
        assert!(
            video
                .plan(
                    MediaChoice::Video {
                        max_height: Some(720)
                    },
                    false
                )
                .is_err()
        );
    }

    #[test]
    fn webm_pictures_join_webm_sound_and_mixed_ones_matroska() {
        let mut video = video();
        video
            .formats
            .retain(|f| !["137", "136"].contains(&f.format_id.as_str()));
        let webm = video.plan(MediaChoice::default(), true).unwrap();
        assert_eq!(
            (ids(&webm), webm.recipe),
            ("248+251", Recipe::Join(Container::Webm))
        );
        video.formats.retain(|f| f.format_id != "251");
        let mixed = video.plan(MediaChoice::default(), true).unwrap();
        assert_eq!(
            (ids(&mixed), mixed.recipe, mixed.extension.as_str()),
            ("248+140", Recipe::Join(Container::Mkv), "mkv")
        );
    }

    #[test]
    fn audio_choices_take_the_best_sound() {
        let video = video();
        let mp3 = video
            .plan(
                MediaChoice::Audio {
                    format: AudioFormat::Mp3,
                },
                true,
            )
            .unwrap();
        assert_eq!(
            (ids(&mp3), mp3.recipe, mp3.extension.as_str()),
            ("251", Recipe::Mp3, "mp3")
        );
        let m4a = video
            .plan(
                MediaChoice::Audio {
                    format: AudioFormat::M4a,
                },
                false,
            )
            .unwrap();
        assert_eq!(
            (ids(&m4a), m4a.recipe, m4a.extension.as_str()),
            ("140", Recipe::Copy, "m4a")
        );
        assert!(
            video
                .plan(
                    MediaChoice::Audio {
                        format: AudioFormat::Mp3
                    },
                    false
                )
                .is_err()
        );
    }

    #[test]
    fn live_streams_playlists_and_videos_without_whole_formats_are_refused() {
        for (key, value) in [
            ("live_status", json!("is_live")),
            ("_type", json!("playlist")),
            ("formats", json!([])),
        ] {
            let mut raw = serde_json::to_value(json!({"id": "abcdefghijk", "formats": [format("18", "mp4", "avc1", "mp4a", Some(360), 1.0)]})).unwrap();
            raw[key] = value;
            let video: Video = serde_json::from_value(raw).unwrap();
            let err = video.info().unwrap_err();
            assert_eq!(err.code, FailureCode::UnsupportedCapability, "{key}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn yt_dlp_runs_without_configuration_and_its_answer_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let answer = dir.path().join("answer.json");
        let raw = json!({"id": "abcdefghijk", "title": "T", "formats": [format("18", "mp4", "avc1", "mp4a", Some(360), 1.0)]});
        std::fs::write(&answer, raw.to_string()).unwrap();
        let args = dir.path().join("args");
        let body = format!(
            "if [ \"$2\" = --version ]; then echo 2026.09.30; exit 0; fi\nprintf '%s\\n' \"$@\" > '{}'\ncat '{}'",
            args.display(),
            answer.display()
        );
        let path = crate::plugin::tests::script(dir.path(), "yt-dlp", &body);
        let ytdlp = YtDlp::detect(Some(path), None, Some("quickjs".into())).await;
        let summary = ytdlp.plugin().summary(Vec::new());
        assert_eq!(summary.version.as_deref(), Some("2026.09.30"));
        let page = Url::parse("https://www.youtube.com/watch?v=abcdefghijk").unwrap();
        let info = ytdlp.describe(&page).await.unwrap().info().unwrap();
        assert_eq!(info.title, "T");
        let args = std::fs::read_to_string(args).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "--ignore-config",
                "--no-playlist",
                "--no-warnings",
                "--no-progress",
                "--socket-timeout",
                "20",
                "-J",
                "--no-cache-dir",
                "--js-runtimes",
                "quickjs",
                "--",
                "https://www.youtube.com/watch?v=abcdefghijk",
            ]
        );
    }

    #[test]
    fn errors_name_what_happened_without_the_tool_prefix() {
        let failed = |stderr: &str| {
            failure(RunError::Failed {
                status: Some(1),
                stderr: stderr.into(),
            })
        };
        let gone = failed(
            "WARNING: x\nERROR: [youtube] abcdefghijk: Video unavailable. This video has been removed\n",
        );
        assert_eq!(gone.code, FailureCode::ResourceDeleted);
        assert_eq!(
            gone.message,
            "Video unavailable. This video has been removed"
        );
        let bot = failed("ERROR: [youtube] abcdefghijk: Sign in to confirm you're not a bot");
        assert_eq!(bot.code, FailureCode::ChallengeRequired);
        let other = failed("Traceback\nboom");
        assert_eq!(
            (other.code, other.message.as_str()),
            (FailureCode::AdapterParseFailure, "boom")
        );
        assert_eq!(
            failure(RunError::Missing).code,
            FailureCode::UnsupportedCapability
        );
    }
}
