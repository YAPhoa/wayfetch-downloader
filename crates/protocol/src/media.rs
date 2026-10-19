//! Media links: links to a video page that a media service resolves into
//! streams, such as a YouTube watch page, and the helper programs
//! (plugins) that service needs. Added in protocol 4.0.
//!
//! A collected media link is checked once in the background: the engine
//! asks its media service for the title and the formats, and announces the
//! answer with `links_changed`. Before starting it, the person chooses a
//! video quality or an audio-only format; the choice is kept with the
//! link, then with its download.
//!
//! Protocol 4.1 lets Settings turn a plugin off and on, update one that
//! Wayfetch can update from its official signed releases, and remove the
//! copy Wayfetch installed ([`crate::Command::ChangePlugin`]).

use serde::{Deserialize, Serialize};
use wayfetch_domain::name::{DiagnosticText, PluginId, ProviderId};

/// What to make of a media link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum MediaChoice {
    /// The video with its sound, at the best quality not taller than
    /// `max_height` pixels (any height if absent).
    Video {
        /// The tallest acceptable picture, such as 1080.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        max_height: Option<u32>,
    },
    /// Only the sound, in `format`.
    Audio {
        /// The audio file's format.
        format: AudioFormat,
    },
}

impl Default for MediaChoice {
    /// The best video.
    fn default() -> Self {
        Self::Video { max_height: None }
    }
}

/// An audio-only file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum AudioFormat {
    /// MP3, converted from the best sound.
    Mp3,
    /// The sound as the service serves it, in an M4A file, unconverted.
    M4a,
}

/// Whether a media link has been checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum MediaState {
    /// Waiting for, or in, the check.
    Checking,
    /// Checked: the title and formats are known.
    Ready,
    /// The check failed; `problem` says why. Starting it tries again.
    Unavailable,
}

/// A choice a media link offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MediaOption {
    /// The choice.
    pub choice: MediaChoice,
    /// Whether it can be made now; it cannot while a plugin it needs is
    /// missing, and `needs` then names it.
    pub available: bool,
}

/// What is known about a collected media link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LinkMedia {
    /// The media service that recognized the link.
    pub service: ProviderId,
    /// Whether it has been checked.
    pub state: MediaState,
    /// The video's title, once checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub title: Option<String>,
    /// Its length in seconds, if the service said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub duration_seconds: Option<u64>,
    /// What it will be downloaded as.
    pub choice: MediaChoice,
    /// The choices it offers, once checked: the best video first, then
    /// each picture height the video has, tallest first, then audio only.
    pub options: Vec<MediaOption>,
    /// What an unavailable option needs, such as a missing plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub needs: Option<DiagnosticText>,
    /// Why the check failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub problem: Option<DiagnosticText>,
}

/// How a download of a media link is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobMedia {
    /// The media service.
    pub service: ProviderId,
    /// What it is downloaded as.
    pub choice: MediaChoice,
}

/// What a plugin does for Wayfetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum PluginRole {
    /// Reads media pages: titles, formats, and stream links.
    MediaResolver,
    /// Joins picture and sound streams and converts audio.
    MediaConverter,
}

/// Whether a plugin can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum PluginState {
    /// Found and working.
    Ready,
    /// Not installed, or not where the configuration says.
    Missing,
    /// Found, but it did not run; `problem` says why.
    Failed,
}

/// A helper program Wayfetch runs, as the Plugins settings list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PluginSummary {
    /// Its stable ID, such as `yt-dlp`.
    pub id: String,
    /// Its name.
    pub name: String,
    /// What it does.
    pub role: PluginRole,
    /// The media services that use it.
    pub services: Vec<ProviderId>,
    /// Whether it can be used.
    pub state: PluginState,
    /// The version it reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub version: Option<String>,
    /// Why it cannot be used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub problem: Option<DiagnosticText>,
    /// Whether its services use it; a plugin turned off is still checked
    /// and listed. Added in protocol 4.1; true when absent.
    #[serde(default = "on")]
    pub enabled: bool,
    /// Whether Settings can install a newer official release of it. Added
    /// in protocol 4.1.
    #[serde(default)]
    pub updatable: bool,
    /// Whether the copy in use is one Wayfetch installed, which
    /// [`PluginChange::Remove`] deletes. Added in protocol 4.1.
    #[serde(default)]
    pub installed: bool,
    /// While an installed copy is in use: the version of the copy found
    /// without it, used again once it is removed; absent if none was
    /// found or its version is unknown. Added in protocol 4.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fallback_version: Option<String>,
}

fn on() -> bool {
    true
}

impl PluginSummary {
    /// Whether its services can use it: turned on and working.
    pub fn usable(&self) -> bool {
        self.enabled && self.state == PluginState::Ready
    }
}

/// A change to a plugin. Added in protocol 4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum PluginChange {
    /// Turn it on: its services use it again.
    Enable,
    /// Turn it off: its services stop using it; nothing is deleted.
    Disable,
    /// Install its newest official release if that is newer than the copy
    /// in use, after checking the release's signature and that it runs;
    /// if anything fails, the copy in use stays.
    Update,
    /// Delete the copy Wayfetch installed, going back to the copy found
    /// without it, if any.
    Remove,
}

/// What a [`PluginChange`] did. Added in protocol 4.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct PluginChanged {
    /// The plugin.
    pub plugin: PluginId,
    /// Whether anything changed; an update finding nothing newer, or
    /// turning on a plugin already on, changes nothing.
    pub changed: bool,
    /// The version in use afterwards, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn choices_have_a_stable_wire_form() {
        let cases = [
            (MediaChoice::default(), json!({"kind": "video"})),
            (
                MediaChoice::Video {
                    max_height: Some(720),
                },
                json!({"kind": "video", "max_height": 720}),
            ),
            (
                MediaChoice::Audio {
                    format: AudioFormat::Mp3,
                },
                json!({"kind": "audio", "format": "mp3"}),
            ),
        ];
        for (choice, wire) in cases {
            assert_eq!(serde_json::to_value(choice).unwrap(), wire);
            assert_eq!(serde_json::from_value::<MediaChoice>(wire).unwrap(), choice);
        }
        assert!(serde_json::from_value::<MediaChoice>(json!({"kind": "video", "x": 1})).is_err());
        assert!(serde_json::from_value::<MediaChoice>(json!({"kind": "audio"})).is_err());
    }
}
