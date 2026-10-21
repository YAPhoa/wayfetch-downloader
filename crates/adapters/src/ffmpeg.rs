//! ffmpeg as the media converter (ADR-27): joins a picture stream with its
//! sound without re-encoding, or converts sound to MP3.
//!
//! The streams are read in place from the engine's staging file through
//! ffmpeg's `subfile` protocol, and only the `subfile` and `file`
//! protocols are allowed for them, so a path is never read as a network
//! address. ffmpeg writes the output file the engine created, in place.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use wayfetch_engine::domain::failure::FailureCode;
use wayfetch_engine::media::{Container, Conversion, Converter, MediaError, Part, Recipe};
use wayfetch_engine::protocol::media::{PluginRole, PluginSummary};
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::plugin::{Check, Describe, Limits, Plugin};

/// The plugin.
pub const PLUGIN: Describe = Describe {
    id: "ffmpeg",
    name: "ffmpeg",
    role: PluginRole::MediaConverter,
};

/// Longest one conversion may take: converting hours of sound to MP3
/// takes minutes.
const DEADLINE: Duration = Duration::from_secs(4 * 60 * 60);

/// ffmpeg, found and checked.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    plugin: Arc<Plugin>,
}

impl Ffmpeg {
    /// How ffmpeg is checked.
    pub fn check() -> Check {
        Check {
            args: &["-hide_banner", "-version"],
            version: |out| {
                // `ffmpeg version 7.1.1 Copyright ...`
                let version = out.lines().next()?.strip_prefix("ffmpeg version ")?;
                version.split_whitespace().next().map(str::to_owned)
            },
            env: Vec::new(),
            confinement: None,
        }
    }

    /// ffmpeg as `plugin`.
    pub fn new(plugin: Arc<Plugin>) -> Self {
        Self { plugin }
    }

    /// ffmpeg at `path`, if found, checked by asking its version.
    pub async fn detect(path: Option<PathBuf>) -> Self {
        Self::new(Plugin::detect(PLUGIN, Self::check(), path).await)
    }

    /// The plugin.
    pub fn plugin(&self) -> &Arc<Plugin> {
        &self.plugin
    }
}

/// `part` of `file` as an ffmpeg input.
fn subfile(file: &Path, part: Part) -> String {
    format!(
        "subfile,,start,{},end,{},,:file:{}",
        part.offset,
        part.offset + part.length,
        file.display()
    )
}

/// ffmpeg's arguments for `conversion`, or `None` for a recipe that
/// converts nothing.
pub fn arguments(conversion: &Conversion<'_>) -> Option<Vec<String>> {
    let mut args: Vec<String> = ["-hide_banner", "-nostdin", "-loglevel", "error", "-y"]
        .map(str::to_owned)
        .into();
    for part in conversion.parts {
        args.extend([
            "-protocol_whitelist".into(),
            "subfile,file".into(),
            "-i".into(),
            subfile(conversion.input, *part),
        ]);
    }
    let format = match conversion.recipe {
        Recipe::Copy => return None,
        Recipe::Join(container) => {
            args.extend(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"].map(str::to_owned));
            match container {
                // The index first, so players start before the end arrives.
                Container::Mp4 => {
                    args.extend(["-movflags".into(), "+faststart".into()]);
                    "mp4"
                }
                Container::Webm => "webm",
                Container::Mkv | Container::Mp3 => "matroska",
            }
        }
        Recipe::Mp3 => {
            args.extend(
                ["-map", "0:a:0", "-vn", "-c:a", "libmp3lame", "-q:a", "2"].map(str::to_owned),
            );
            "mp3"
        }
    };
    args.extend([
        "-metadata".into(),
        format!("title={}", conversion.title),
        "-f".into(),
        format.into(),
        format!("file:{}", conversion.output.display()),
    ]);
    Some(args)
}

impl Converter for Ffmpeg {
    fn plugin(&self) -> PluginSummary {
        self.plugin.summary(Vec::new())
    }

    fn convert<'a>(&'a self, conversion: Conversion<'a>) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            let args = arguments(&conversion).ok_or_else(|| {
                MediaError::new(FailureCode::UnsupportedCapability, "nothing to convert")
            })?;
            let limits = Limits {
                deadline: DEADLINE,
                stdout_bytes: 64 * 1024,
            };
            self.plugin
                .program()
                .run(&args, limits)
                .await
                .map(drop)
                .map_err(|e| {
                    MediaError::new(
                        FailureCode::IntegrityMismatch,
                        format!("ffmpeg could not make the file: {e}"),
                    )
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversion<'a>(parts: &'a [Part], recipe: Recipe) -> Conversion<'a> {
        Conversion {
            input: Path::new("/state/root/.wayfetch/staging/7.g1.part"),
            parts,
            output: Path::new("/state/root/.wayfetch/staging/7.g1.media"),
            recipe,
            title: "A, b; c",
        }
    }

    #[test]
    fn joins_read_both_parts_of_the_staging_file_and_copy_them() {
        let parts = [
            Part {
                offset: 0,
                length: 100,
            },
            Part {
                offset: 100,
                length: 20,
            },
        ];
        let args = arguments(&conversion(&parts, Recipe::Join(Container::Mp4))).unwrap();
        assert_eq!(
            args.join(" "),
            "-hide_banner -nostdin -loglevel error -y \
             -protocol_whitelist subfile,file -i subfile,,start,0,end,100,,:file:/state/root/.wayfetch/staging/7.g1.part \
             -protocol_whitelist subfile,file -i subfile,,start,100,end,120,,:file:/state/root/.wayfetch/staging/7.g1.part \
             -map 0:v:0 -map 1:a:0 -c copy -movflags +faststart \
             -metadata title=A, b; c -f mp4 file:/state/root/.wayfetch/staging/7.g1.media"
        );
        let mkv = arguments(&conversion(&parts, Recipe::Join(Container::Mkv))).unwrap();
        assert!(mkv.windows(2).any(|w| w == ["-f", "matroska"]));
    }

    #[test]
    fn mp3_encodes_the_sound_only() {
        let parts = [Part {
            offset: 0,
            length: 50,
        }];
        let args = arguments(&conversion(&parts, Recipe::Mp3)).unwrap();
        assert!(args.windows(2).any(|w| w == ["-c:a", "libmp3lame"]));
        assert!(args.contains(&"-vn".to_owned()));
        assert!(args.windows(2).any(|w| w == ["-f", "mp3"]));
        assert!(arguments(&conversion(&parts, Recipe::Copy)).is_none());
    }
}
