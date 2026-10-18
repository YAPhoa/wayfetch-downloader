//! The seams between the engine and media services: links to a video page
//! that resolve into one or more streams, such as YouTube watch pages
//! (architecture brief ADR-27; protocol 4.0).
//!
//! * A [`MediaService`] recognizes its links without the network, checks
//!   one (title and formats) in the background after it is collected, and
//!   resolves one into the streams for a [`MediaChoice`] when its download
//!   starts. Each resolve answers fresh stream links: they expire, so none
//!   is stored.
//! * A [`Converter`] joins a video stream with its sound, or converts
//!   sound to MP3, reading the downloaded streams from the staging file and
//!   writing a second staging file the engine created.
//!
//! The engine still downloads every stream itself, through the transport,
//! with its checkpoints, speed limit, and network policy; the streams of a
//! download are laid end to end in its one staging file, so resume and
//! cleanup work as for any download. A stream's identity is the media
//! service's description of what was chosen (video, format, and length),
//! not its expiring URL, so a download continues after its links expire
//! as long as the service still serves the same formats.

use std::path::Path;

use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::name::ProviderId;
use wayfetch_protocol::media::{AudioFormat, MediaChoice, MediaOption, PluginState, PluginSummary};

use crate::fs::RelativePath;
use crate::transfer::writer::BoxFuture;

/// Longest identity a plan may carry, leaving room in the staging
/// record's 512-byte resource for the engine's description of the layout.
pub const MAX_IDENTITY_BYTES: usize = 384;

/// Longest file name extension a plan may name.
pub const MAX_EXTENSION_BYTES: usize = 8;

/// Most streams one download combines.
pub const MAX_STREAMS: usize = 2;

/// What checking a media link found.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaInfo {
    /// The title.
    pub title: String,
    /// The length in seconds, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    /// Every picture height a video stream has, tallest first.
    pub heights: Vec<u32>,
    /// The heights served with sound in one stream, which need no joining.
    pub progressive: Vec<u32>,
    /// Whether a sound-only stream exists, for MP3.
    pub audio: bool,
    /// Whether a sound-only M4A stream exists.
    pub m4a: bool,
}

impl MediaInfo {
    /// Most heights kept, so the stored answer stays small.
    pub const MAX_HEIGHTS: usize = 16;

    /// The info with its lists sorted, deduplicated, and bounded, and the
    /// title trimmed to at most 300 characters.
    pub fn normalized(mut self) -> Self {
        for list in [&mut self.heights, &mut self.progressive] {
            list.sort_unstable_by(|a, b| b.cmp(a));
            list.dedup();
            list.truncate(Self::MAX_HEIGHTS);
        }
        self.title = self
            .title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(300)
            .collect::<String>()
            .trim()
            .to_owned();
        self
    }

    /// The height a video choice makes: the tallest not above its limit,
    /// or the shortest there is when every height is above it. Without a
    /// converter, only heights served with sound count.
    pub fn height_for(&self, max_height: Option<u32>, converter: bool) -> Option<u32> {
        let heights = if converter {
            &self.heights
        } else {
            &self.progressive
        };
        let fits = |h: &&u32| max_height.is_none_or(|max| **h <= max);
        heights
            .iter()
            .find(fits)
            .or_else(|| heights.last())
            .copied()
    }

    /// Whether `choice` can be made, with or without a converter. Without
    /// one, the best video is the best served with sound, but a height
    /// limit is kept only if the height it would make with a converter is
    /// served with sound.
    pub fn can_make(&self, choice: MediaChoice, converter: bool) -> bool {
        match choice {
            MediaChoice::Video { max_height } => {
                let made = self.height_for(max_height, converter);
                made.is_some()
                    && (converter
                        || max_height.is_none()
                        || made == self.height_for(max_height, true))
            }
            MediaChoice::Audio {
                format: AudioFormat::Mp3,
            } => self.audio && converter,
            MediaChoice::Audio {
                format: AudioFormat::M4a,
            } => self.m4a,
        }
    }

    /// The choices to offer: the best video, each height, then audio.
    pub fn options(&self, converter: bool) -> Vec<MediaOption> {
        let mut choices = vec![MediaChoice::default()];
        choices.extend(self.heights.iter().map(|&h| MediaChoice::Video {
            max_height: Some(h),
        }));
        if self.audio {
            choices.push(MediaChoice::Audio {
                format: AudioFormat::Mp3,
            });
        }
        if self.m4a {
            choices.push(MediaChoice::Audio {
                format: AudioFormat::M4a,
            });
        }
        choices
            .into_iter()
            .map(|choice| MediaOption {
                choice,
                available: self.can_make(choice, converter),
            })
            .collect()
    }
}

/// One stream of a plan. Its URL is a short-lived capability: it never
/// prints, and the engine never stores it.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaStream {
    /// Where its bytes are.
    pub url: Url,
    /// Its exact length; a stream of unknown length is not offered.
    pub length: u64,
    /// The `User-Agent` the link was made for, if it needs one.
    pub user_agent: Option<String>,
    /// The most bytes to ask for in one request, for a host that slows
    /// longer requests down; `None` asks for the rest at once.
    pub chunk_bytes: Option<u64>,
}

impl std::fmt::Debug for MediaStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaStream")
            .field("url", &"[redacted]")
            .field("length", &self.length)
            .field("chunk_bytes", &self.chunk_bytes)
            .finish_non_exhaustive()
    }
}

/// A container a converter writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::Display, strum::EnumString)]
#[strum(serialize_all = "lowercase")]
pub enum Container {
    /// MP4.
    Mp4,
    /// WebM.
    Webm,
    /// Matroska.
    Mkv,
    /// MP3 audio, converted.
    Mp3,
}

/// What the converter makes of a plan's streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipe {
    /// The one stream is the file: nothing to convert.
    Copy,
    /// Join the first stream's picture with the second stream's sound into
    /// a container, without re-encoding.
    Join(Container),
    /// Convert the one stream's sound to MP3.
    Mp3,
}

impl Recipe {
    /// Whether the recipe needs a converter.
    pub fn converts(self) -> bool {
        self != Self::Copy
    }
}

/// How to make a media download's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlan {
    /// What was chosen, such as the video ID, its formats, and their
    /// lengths: equal identities serve equal bytes, so a download continues
    /// across new stream links only while it stays the same. At most
    /// [`MAX_IDENTITY_BYTES`].
    pub identity: String,
    /// The title, for the file name.
    pub title: String,
    /// The file name extension, without a dot.
    pub extension: String,
    /// The streams, in the order they are laid in the staging file; one or
    /// [`MAX_STREAMS`].
    pub streams: Vec<MediaStream>,
    /// What to make of them.
    pub recipe: Recipe,
}

impl MediaPlan {
    /// The plan's total length, if it is a plan the engine can follow.
    pub fn checked_length(&self) -> Result<u64, MediaError> {
        let streams = self.streams.len();
        let expected = match self.recipe {
            Recipe::Join(_) => 2,
            Recipe::Copy | Recipe::Mp3 => 1,
        };
        if streams != expected
            || self.identity.is_empty()
            || self.identity.len() > MAX_IDENTITY_BYTES
            || self.extension.is_empty()
            || self.extension.len() > MAX_EXTENSION_BYTES
            || !self.extension.bytes().all(|b| b.is_ascii_alphanumeric())
            || self
                .streams
                .iter()
                .any(|s| s.length == 0 || s.chunk_bytes == Some(0))
        {
            return Err(MediaError::new(
                FailureCode::AdapterParseFailure,
                "the media service answered a plan Wayfetch cannot follow",
            ));
        }
        self.streams
            .iter()
            .try_fold(0u64, |total, s| total.checked_add(s.length))
            .ok_or_else(|| MediaError::new(FailureCode::AdapterParseFailure, "streams too long"))
    }
}

/// A media service or converter failed. Its message names no stream link
/// and says what the person can do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct MediaError {
    /// The failure to record.
    pub code: FailureCode,
    /// What happened.
    pub message: String,
}

impl MediaError {
    /// A failure.
    pub fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// One media service.
pub trait MediaService: Send + Sync + 'static {
    /// The service, as named in the UI's catalogue.
    fn service(&self) -> &ProviderId;

    /// Whether `link` is one of the service's media links. Pure: no
    /// network.
    fn recognizes(&self, link: &Url) -> bool;

    /// The plugins it runs, and whether each can be used.
    fn plugins(&self) -> Vec<PluginSummary>;

    /// Whether its plugins can be used.
    fn ready(&self) -> bool {
        self.plugins().iter().all(|p| p.state == PluginState::Ready)
    }

    /// Reads the title and formats of `link`, without downloading.
    fn probe<'a>(&'a self, link: &'a Url) -> BoxFuture<'a, Result<MediaInfo, MediaError>>;

    /// The streams for `choice` of `link`, with fresh links; `converter`
    /// says whether a plan may need converting.
    fn resolve<'a>(
        &'a self,
        link: &'a Url,
        choice: MediaChoice,
        converter: bool,
    ) -> BoxFuture<'a, Result<MediaPlan, MediaError>>;
}

/// One downloaded stream in the staging file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    /// Where it starts.
    pub offset: u64,
    /// Its length.
    pub length: u64,
}

/// A conversion to run.
#[derive(Debug)]
pub struct Conversion<'a> {
    /// The staging file holding the streams.
    pub input: &'a Path,
    /// The streams in it, in plan order.
    pub parts: &'a [Part],
    /// The output file, created empty by the engine; the converter writes
    /// it in place.
    pub output: &'a Path,
    /// What to make.
    pub recipe: Recipe,
    /// The title, for the file's tags.
    pub title: &'a str,
}

/// Joins and converts media streams.
pub trait Converter: Send + Sync + 'static {
    /// The plugin, and whether it can be used.
    fn plugin(&self) -> PluginSummary;

    /// Whether it can be used.
    fn ready(&self) -> bool {
        self.plugin().state == PluginState::Ready
    }

    /// Runs `conversion`. Dropping the future stops it.
    fn convert<'a>(&'a self, conversion: Conversion<'a>) -> BoxFuture<'a, Result<(), MediaError>>;
}

/// Characters a file name never holds on any platform Wayfetch saves to.
const UNSAFE: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// A file name for `title` with `extension` (none for a name without
/// one), with characters no platform allows replaced, at most 200 bytes
/// before the extension. `None` if nothing is left.
pub fn title_file_name(title: &str, extension: Option<&str>) -> Option<RelativePath> {
    let mut stem = String::new();
    for c in title.chars() {
        let c = if c.is_control() || UNSAFE.contains(&c) {
            '_'
        } else {
            c
        };
        if stem.len() + c.len_utf8() > 200 {
            break;
        }
        stem.push(c);
    }
    // Trailing dots and spaces are dropped by Windows and hide extensions.
    let stem = stem
        .trim()
        .trim_end_matches(['.', ' '])
        .trim_start_matches('.');
    if stem.is_empty() {
        return None;
    }
    let name = match extension {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    };
    crate::download::file_name(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> MediaInfo {
        MediaInfo {
            title: "A".into(),
            duration_seconds: Some(60),
            heights: vec![360, 1080, 720, 1080],
            progressive: vec![360],
            audio: true,
            m4a: true,
        }
        .normalized()
    }

    #[test]
    fn heights_are_sorted_and_choices_pick_the_tallest_that_fits() {
        let info = info();
        assert_eq!(info.heights, [1080, 720, 360]);
        assert_eq!(info.height_for(None, true), Some(1080));
        assert_eq!(info.height_for(Some(1000), true), Some(720));
        assert_eq!(info.height_for(Some(144), true), Some(360));
        assert_eq!(info.height_for(None, false), Some(360));
    }

    #[test]
    fn without_a_converter_only_single_streams_are_offered() {
        let info = info();
        let available = |converter| {
            info.options(converter)
                .into_iter()
                .filter(|o| o.available)
                .map(|o| o.choice)
                .collect::<Vec<_>>()
        };
        assert_eq!(available(true).len(), 6);
        assert_eq!(
            available(false),
            [
                MediaChoice::default(),
                MediaChoice::Video {
                    max_height: Some(360)
                },
                MediaChoice::Audio {
                    format: AudioFormat::M4a
                },
            ]
        );
    }

    #[test]
    fn titles_become_safe_file_names() {
        let name = |t, e| title_file_name(t, e).map(|n| n.as_str().to_owned());
        assert_eq!(
            name("AC/DC: Live? <Best>", Some("mp4")).as_deref(),
            Some("AC_DC_ Live_ _Best_.mp4")
        );
        assert_eq!(name("  ..hidden. ", None).as_deref(), Some("hidden"));
        assert_eq!(name("///", Some("mp3")).as_deref(), Some("___.mp3"));
        assert_eq!(name("  ", Some("mp3")), None);
        let long = "é".repeat(300);
        assert!(name(&long, Some("webm")).unwrap().len() <= 205);
    }

    #[test]
    fn plans_must_match_their_recipe() {
        let stream = |length| MediaStream {
            url: Url::parse("https://cdn.example/a").unwrap(),
            length,
            user_agent: None,
            chunk_bytes: None,
        };
        let plan = |streams, recipe| MediaPlan {
            identity: "yt:x".into(),
            title: "t".into(),
            extension: "mp4".into(),
            streams,
            recipe,
        };
        assert_eq!(
            plan(vec![stream(3), stream(4)], Recipe::Join(Container::Mp4))
                .checked_length()
                .unwrap(),
            7
        );
        assert!(
            plan(vec![stream(3)], Recipe::Join(Container::Mp4))
                .checked_length()
                .is_err()
        );
        assert!(
            plan(vec![stream(3), stream(4)], Recipe::Copy)
                .checked_length()
                .is_err()
        );
        assert!(plan(vec![stream(0)], Recipe::Mp3).checked_length().is_err());
        assert!(
            plan(
                vec![stream(u64::MAX), stream(1)],
                Recipe::Join(Container::Mkv)
            )
            .checked_length()
            .is_err()
        );
    }
}
