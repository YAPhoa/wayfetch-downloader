//! The ffmpeg converter against the ffmpeg on `PATH`, with synthetic
//! media it generates itself: a picture stream and a sound stream laid end
//! to end in one file, as the engine's staging file holds them, are joined
//! into MP4 and converted to MP3. Run with `--ignored` where ffmpeg and
//! ffprobe are installed.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use wayfetch_adapters::ffmpeg::Ffmpeg;
use wayfetch_adapters::plugin::find_on_path;
use wayfetch_engine::media::{Container, Conversion, Converter, Part, Recipe};

fn generate(dir: &Path, name: &str, args: &[&str]) -> Vec<u8> {
    let path = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::read(path).unwrap()
}

/// `codec_type:codec_name` of each stream of `file`.
fn streams(file: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name",
        ])
        .args(["-of", "csv=p=0"])
        .arg(file)
        .output()
        .unwrap();
    assert!(out.status.success());
    let mut found: Vec<_> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let (name, kind) = l.split_once(',').unwrap();
            format!("{kind}:{name}")
        })
        .collect();
    found.sort();
    found
}

#[tokio::test]
#[ignore = "runs the ffmpeg and ffprobe on PATH"]
async fn streams_in_one_file_are_joined_and_converted() {
    let ffmpeg = Ffmpeg::detect(find_on_path("ffmpeg")).await;
    assert!(ffmpeg.plugin().usable(), "{:?}", ffmpeg.plugin());
    let dir = tempfile::tempdir().unwrap();
    let picture = generate(
        dir.path(),
        "picture.mp4",
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x48:rate=10",
            "-an",
            "-c:v",
            "mpeg4",
        ],
    );
    let sound = generate(
        dir.path(),
        "sound.m4a",
        &["-f", "lavfi", "-i", "sine=duration=1", "-vn", "-c:a", "aac"],
    );
    // A path with a comma and a colon, which ffmpeg must not parse.
    let staging = dir.path().join("7.g1, x:y.part");
    std::fs::write(&staging, [picture.as_slice(), sound.as_slice()].concat()).unwrap();
    let parts = [
        Part {
            offset: 0,
            length: picture.len() as u64,
        },
        Part {
            offset: picture.len() as u64,
            length: sound.len() as u64,
        },
    ];

    let output = dir.path().join("7.g1.media");
    std::fs::write(&output, b"").unwrap();
    let conversion = Conversion {
        input: &staging,
        parts: &parts,
        output: &output,
        recipe: Recipe::Join(Container::Mp4),
        title: "Synthetic: test",
    };
    let inode =
        |path: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).unwrap());
    let created = inode(&output);
    ffmpeg.convert(conversion).await.unwrap();
    assert_eq!(streams(&output), ["audio:aac", "video:mpeg4"]);
    // Written in place, index moved first included, so the engine's
    // record of the file it created still names it.
    assert_eq!(inode(&output), created);

    let mp3 = dir.path().join("7.g2.media");
    std::fs::write(&mp3, b"").unwrap();
    let conversion = Conversion {
        input: &staging,
        parts: &parts[1..],
        output: &mp3,
        recipe: Recipe::Mp3,
        title: "Synthetic: test",
    };
    ffmpeg.convert(conversion).await.unwrap();
    assert_eq!(streams(&mp3), ["audio:mp3"]);

    // A part that is not media fails, saying so.
    let garbage = Part {
        offset: 0,
        length: 16,
    };
    let conversion = Conversion {
        input: &staging,
        parts: &[garbage, garbage],
        output: &output,
        recipe: Recipe::Join(Container::Webm),
        title: "x",
    };
    let err = ffmpeg.convert(conversion).await.unwrap_err();
    assert!(
        err.message.starts_with("ffmpeg could not make the file"),
        "{err}"
    );
}
