//! 7-Zip as the archive extractor (ADR-18, ADR-19) for ZIP, 7z, RAR, and tar
//! archives.
//!
//! Every run is confined ([`Confinement`]), takes passwords on standard
//! input, refuses dangerous links, and caps its memory. Compressed tars are
//! piped between two runs so the tar is never written. Releases before 25.01
//! are refused for their unsafe link handling.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use wayfetch_engine::accounts::Secret;
use wayfetch_engine::domain::name::DiagnosticText;
use wayfetch_engine::extract::{
    Archive, Entry, EntryKind, ExtractError, Extractor, Halt, Layout, MAX_ENTRIES,
};
use wayfetch_engine::protocol::media::{PluginRole, PluginState, PluginSummary};
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::plugin::{Check, Confinement, Describe, Limits, Plugin, RunError, last_line};

/// The plugin.
pub const PLUGIN: Describe = Describe {
    id: "7zip",
    name: "7-Zip",
    role: PluginRole::ArchiveExtractor,
};

/// The oldest release used: the first without the link vulnerabilities.
const OLDEST: (u32, u32) = (25, 1);

/// Longest a listing may take.
const LIST_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// Longest an extraction may take: unpacking tens of gigabytes from a
/// slow disk takes hours.
const EXTRACT_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);

/// The most listing output read: well over [`MAX_ENTRIES`] entries'
/// worth.
const LIST_BYTES: usize = MAX_ENTRIES * 1024;

/// The most memory unpacking may take, in gigabytes: enough for the
/// dictionaries archives use in practice.
const UNPACK_MEMORY_GB: u64 = 2;

/// The most address space a run may map: what unpacking may take, and
/// room for 7-Zip itself and its threads.
pub const MEMORY_BYTES: u64 = (UNPACK_MEMORY_GB + 2) << 30;

/// 7-Zip, found and checked.
#[derive(Debug, Clone)]
pub struct SevenZip {
    plugin: Arc<Plugin>,
}

impl SevenZip {
    /// How 7-Zip is checked: with no arguments it prints its version.
    pub fn check() -> Check {
        Check {
            args: &[],
            version,
            env: Vec::new(),
            confinement: Some(Confinement::new(MEMORY_BYTES)),
        }
    }

    /// 7-Zip as `plugin`.
    pub fn new(plugin: Arc<Plugin>) -> Self {
        Self { plugin }
    }

    /// 7-Zip at `path`, if found, checked by asking its version.
    pub async fn detect(path: Option<PathBuf>) -> Self {
        Self::new(Plugin::detect(PLUGIN, Self::check(), path).await)
    }

    /// The plugin.
    pub fn plugin(&self) -> &Arc<Plugin> {
        &self.plugin
    }
}

/// The version in 7-Zip's banner: `7-Zip (z) 26.04 (x64) : ...`, or
/// `7-Zip 23.01 (x64) : ...` for the distributions' build.
fn version(out: &str) -> Option<String> {
    let line = out.lines().find_map(|l| l.trim().strip_prefix("7-Zip "))?;
    let line = line
        .strip_prefix("(z) ")
        .or(line.strip_prefix("(a) "))
        .unwrap_or(line);
    let version = line.split_whitespace().next()?;
    numbers(version).map(|_| version.to_owned())
}

/// `major.minor` as numbers.
fn numbers(version: &str) -> Option<(u32, u32)> {
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// What Settings shows for 7-Zip at `version`: a release older than
/// [`OLDEST`] is not used.
fn too_old(summary: &mut PluginSummary) {
    let old = summary.state == PluginState::Ready
        && summary
            .version
            .as_deref()
            .is_none_or(|v| numbers(v).is_none_or(|n| n < OLDEST));
    if old {
        summary.state = PluginState::Failed;
        summary.problem = Some(DiagnosticText::new(&format!(
            "7-Zip {} is too old; version {}.{:02} or newer is needed.",
            summary.version.as_deref().unwrap_or("of unknown version"),
            OLDEST.0,
            OLDEST.1
        )));
    }
}

/// The switches every run takes: no prompts or progress, the password
/// from standard input if there is one, else none.
fn common(archive: &Archive<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-y", "-bso0", "-bsp0"].map(Into::into).into();
    if archive.password.is_none() {
        args.push("-p".into());
    }
    args
}

/// 7-Zip's arguments to list `archive`.
fn list_arguments(archive: &Archive<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["l", "-slt", "-ba"].map(Into::into).into();
    args.extend(common(archive).into_iter().filter(|a| a != "-bso0"));
    args.extend(["--".into(), archive.path.into()]);
    args
}

/// 7-Zip's arguments to unpack `archive` into `output`, or, for a
/// compressed tar, the arguments that decompress it and those that unpack
/// the tar from standard input.
fn extract_arguments(archive: &Archive<'_>, output: &Path) -> (Vec<OsString>, Vec<OsString>) {
    let mut unpack: Vec<OsString> = vec!["x".into(), output_switch(output)];
    unpack.extend(common(archive));
    unpack.extend(
        [
            "-aou".to_owned(),
            "-snld0".to_owned(),
            format!("-smemx{UNPACK_MEMORY_GB}g"),
        ]
        .map(Into::into),
    );
    match archive.layout {
        Layout::Direct => {
            unpack.extend(["--".into(), archive.path.into()]);
            (unpack, Vec::new())
        }
        Layout::CompressedTar => {
            let mut decompress: Vec<OsString> = vec!["x".into(), "-so".into()];
            decompress.extend(common(archive).into_iter().filter(|a| a != "-bso0"));
            decompress.extend(["--".into(), archive.path.into()]);
            unpack.extend(["-si".into(), "-ttar".into()]);
            (decompress, unpack)
        }
    }
}

/// `-o<output>`.
fn output_switch(output: &Path) -> OsString {
    let mut switch = OsString::from("-o");
    switch.push(output);
    switch
}

/// The entries of a `-slt` listing: blocks of `Key = Value` lines.
fn parse_listing(out: &str) -> Result<Vec<Entry>, ExtractError> {
    let mut entries = Vec::new();
    for block in out.split("\n\n") {
        let field = |key: &str| {
            block.lines().find_map(|line| {
                let (k, v) = line.split_once(" = ").or_else(|| {
                    // An empty value has no space after the `=`.
                    line.strip_suffix(" =").map(|k| (k, ""))
                })?;
                (k == key).then(|| v.trim_end_matches('\r'))
            })
        };
        let Some(path) = field("Path") else {
            continue;
        };
        if entries.len() == MAX_ENTRIES {
            return Err(ExtractError::Unsupported(format!(
                "The archive holds more than {MAX_ENTRIES} files."
            )));
        }
        let mode = field("Attributes")
            .and_then(|a| a.split_whitespace().find(|w| w.len() == 10))
            .and_then(|m| m.chars().next());
        let linked = ["Symbolic Link", "Hard Link"]
            .iter()
            .any(|k| field(k).is_some_and(|v| !v.is_empty()));
        let kind = match mode {
            _ if linked => EntryKind::Link,
            Some('l') => EntryKind::Link,
            Some('c' | 'b' | 'p' | 's') => EntryKind::Special,
            Some('d') => EntryKind::Directory,
            _ if field("Folder") == Some("+") => EntryKind::Directory,
            _ => EntryKind::File,
        };
        entries.push(Entry {
            path: path.to_owned(),
            kind,
            size: field("Size").and_then(|s| s.parse().ok()),
            encrypted: field("Encrypted") == Some("+"),
        });
    }
    Ok(entries)
}

/// What a failed run of 7-Zip means, for an archive given `password` or
/// not.
fn failure(error: RunError, password: bool) -> ExtractError {
    let stderr = match &error {
        RunError::Failed {
            status: Some(_),
            stderr,
        } => stderr,
        // Stopped by a signal: past its file size limit, or killed.
        RunError::Failed { status: None, .. } => {
            return ExtractError::Failed(
                "7-Zip was stopped before it finished; the archive may unpack to more than \
                 Wayfetch allows."
                    .into(),
            );
        }
        RunError::Stopped => return ExtractError::Stopped,
        RunError::TooMuchOutput(_) => {
            return ExtractError::Unsupported(format!(
                "The archive holds more than {MAX_ENTRIES} files."
            ));
        }
        other => return ExtractError::Failed(format!("7-Zip stopped: {other}")),
    };
    let says = |text: &str| stderr.contains(text);
    if says("Wrong password") || says("Cannot open encrypted archive") {
        if password {
            ExtractError::WrongPassword
        } else {
            ExtractError::NeedsPassword
        }
    } else if says("Missing volume") {
        ExtractError::MissingVolume
    } else if says("Unexpected end of archive") {
        ExtractError::Damaged("The archive is incomplete or damaged.".into())
    } else if says("Is not archive") || says("Cannot open the file as archive") {
        ExtractError::Damaged("The file is not an archive 7-Zip can read.".into())
    } else if says("Can't allocate required memory") || says("memory usage") {
        ExtractError::Unsupported(format!(
            "The archive needs more than {UNPACK_MEMORY_GB} GB of memory to unpack."
        ))
    } else if says("Unsupported Method") || says("Unsupported archive type") {
        ExtractError::Unsupported("7-Zip cannot unpack this kind of archive.".into())
    } else if says("Data Error") || says("CRC Failed") || says("Headers Error") {
        ExtractError::Damaged("The archive is damaged.".into())
    } else {
        ExtractError::Failed(format!("7-Zip failed: {}", last_line(stderr)))
    }
}

/// A run that may read `archive`'s volumes and write nowhere.
fn reading(archive: &Archive<'_>) -> Confinement {
    Confinement::new(MEMORY_BYTES).read(
        std::iter::once(archive.path)
            .chain(archive.volumes.iter().map(PathBuf::as_path))
            .map(Path::to_path_buf),
    )
}

/// The password as the line 7-Zip reads.
fn password_line(password: &Secret) -> Secret {
    Secret::new([password.expose(), b"\n"].concat())
}

impl Extractor for SevenZip {
    fn plugin(&self) -> PluginSummary {
        let mut summary = self.plugin.summary(Vec::new());
        too_old(&mut summary);
        summary
    }

    fn list<'a>(
        &'a self,
        archive: Archive<'a>,
        halt: Halt,
    ) -> BoxFuture<'a, Result<Vec<Entry>, ExtractError>> {
        Box::pin(async move {
            let limits = Limits {
                deadline: LIST_DEADLINE,
                stdout_bytes: LIST_BYTES,
            };
            let input = archive.password.map(password_line);
            let output = self
                .plugin
                .program()
                .run_with(
                    list_arguments(&archive),
                    input.as_ref().map(Secret::expose),
                    Some(&reading(&archive)),
                    limits,
                    halt,
                )
                .await
                .map_err(|e| failure(e, archive.password.is_some()))?;
            parse_listing(&String::from_utf8_lossy(&output.stdout))
        })
    }

    fn extract<'a>(
        &'a self,
        archive: Archive<'a>,
        output: &'a Path,
        budget: u64,
        halt: Halt,
    ) -> BoxFuture<'a, Result<(), ExtractError>> {
        Box::pin(async move {
            let limits = Limits {
                deadline: EXTRACT_DEADLINE,
                stdout_bytes: 64 * 1024,
            };
            let program = self.plugin.program();
            let ran = match extract_arguments(&archive, output) {
                (unpack, pipe) if pipe.is_empty() => {
                    let input = archive.password.map(password_line);
                    let confinement = reading(&archive).write(output, budget);
                    program
                        .run_with(
                            unpack,
                            input.as_ref().map(Secret::expose),
                            Some(&confinement),
                            limits,
                            halt,
                        )
                        .await
                }
                (decompress, unpack) => {
                    let writing = Confinement::new(MEMORY_BYTES).write(output, budget);
                    program
                        .pipe(
                            (decompress, Some(&reading(&archive))),
                            (unpack, Some(&writing)),
                            limits,
                            halt,
                        )
                        .await
                }
            };
            ran.map(drop)
                .map_err(|e| failure(e, archive.password.is_some()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(layout: Layout, password: Option<&Secret>) -> Archive<'_> {
        Archive {
            path: Path::new("/r/-x.zip"),
            volumes: &[],
            layout,
            password,
        }
    }

    fn joined(args: &[OsString]) -> String {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn versions_are_read_from_either_banner() {
        let ours = "\n7-Zip (z) 26.04 (x64) : Copyright (c) 1999-2026 Igor Pavlov : 2026-10-05\n";
        assert_eq!(version(ours).as_deref(), Some("26.04"));
        let distro = "\n7-Zip 23.01 (x64) : Copyright (c) 1999-2023 Igor Pavlov : 2023-06-20\n";
        assert_eq!(version(distro).as_deref(), Some("23.01"));
        assert_eq!(version("p7zip Version 16.02"), None);
    }

    #[test]
    fn releases_before_25_01_are_not_used() {
        let summary = |version: Option<&str>| {
            let mut summary = PluginSummary {
                id: "7zip".into(),
                name: "7-Zip".into(),
                role: PluginRole::ArchiveExtractor,
                services: Vec::new(),
                state: PluginState::Ready,
                version: version.map(str::to_owned),
                problem: None,
                enabled: true,
                updatable: false,
                installed: false,
                fallback_version: None,
            };
            too_old(&mut summary);
            summary
        };
        assert!(summary(Some("26.04")).usable());
        assert!(summary(Some("25.01")).usable());
        let old = summary(Some("23.01"));
        assert_eq!(old.state, PluginState::Failed);
        assert_eq!(
            old.problem.as_ref().map(DiagnosticText::as_str),
            Some("7-Zip 23.01 is too old; version 25.01 or newer is needed.")
        );
        assert!(!summary(None).usable());
    }

    #[test]
    fn passwords_never_reach_the_command_line() {
        let secret = Secret::from_text("hunter2");
        let with = archive(Layout::Direct, Some(&secret));
        let (unpack, pipe) = extract_arguments(&with, Path::new("/r/.wayfetch/extract/4-g1"));
        assert_eq!(
            joined(&unpack),
            "x -o/r/.wayfetch/extract/4-g1 -y -bso0 -bsp0 -aou -snld0 -smemx2g -- /r/-x.zip"
        );
        assert!(pipe.is_empty());
        assert_eq!(
            joined(&list_arguments(&with)),
            "l -slt -ba -y -bsp0 -- /r/-x.zip"
        );
        assert_eq!(password_line(&secret).expose(), b"hunter2\n");
        // Without one, 7-Zip is told there is none rather than asking.
        let without = archive(Layout::Direct, None);
        assert_eq!(
            joined(&list_arguments(&without)),
            "l -slt -ba -y -bsp0 -p -- /r/-x.zip"
        );
    }

    #[test]
    fn compressed_tars_are_piped_from_one_run_into_another() {
        let tar = archive(Layout::CompressedTar, None);
        let (decompress, unpack) = extract_arguments(&tar, Path::new("/o"));
        assert_eq!(joined(&decompress), "x -so -y -bsp0 -p -- /r/-x.zip");
        assert_eq!(
            joined(&unpack),
            "x -o/o -y -bso0 -bsp0 -p -aou -snld0 -smemx2g -si -ttar"
        );
    }

    #[test]
    fn listings_say_what_each_entry_is() {
        let out = "Path = a.txt\nFolder = -\nSize = 6\nAttributes =  -rw-r--r--\nEncrypted = +\n\n\
                   Path = d\nFolder = +\nSize = 0\nAttributes = D drwxr-xr-x\nEncrypted = -\n\n\
                   Path = link\nFolder = -\nSize = 5\nAttributes =  lrwxrwxrwx\n\n\
                   Path = fifo\nAttributes =  prw-r--r--\n\n\
                   Path = w\\in.txt\nSize = 3\nAttributes = A\n\n\
                   Path = hard\nSize = 0\nHard Link = a.txt\n\n\
                   Path = empty\nSize =\nSymbolic Link =\n";
        let entries = parse_listing(out).unwrap();
        let kinds: Vec<_> = entries.iter().map(|e| (e.path.as_str(), e.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("a.txt", EntryKind::File),
                ("d", EntryKind::Directory),
                ("link", EntryKind::Link),
                ("fifo", EntryKind::Special),
                ("w\\in.txt", EntryKind::File),
                ("hard", EntryKind::Link),
                ("empty", EntryKind::File),
            ]
        );
        assert!(entries[0].encrypted && !entries[1].encrypted);
        assert_eq!(entries[0].size, Some(6));
        assert_eq!(entries[6].size, None);
    }

    #[test]
    fn failures_are_told_apart() {
        let failed = |stderr: &str| RunError::Failed {
            status: Some(2),
            stderr: stderr.into(),
        };
        let encrypted = "ERROR: a.7z : Cannot open encrypted archive. Wrong password?";
        assert_eq!(
            failure(failed(encrypted), false),
            ExtractError::NeedsPassword
        );
        assert_eq!(
            failure(failed(encrypted), true),
            ExtractError::WrongPassword
        );
        assert_eq!(
            failure(failed("ERROR: Missing volume : a.part2.rar"), false),
            ExtractError::MissingVolume
        );
        assert!(matches!(
            failure(failed("ERRORS:\nIs not archive"), false),
            ExtractError::Damaged(_)
        ));
        assert_eq!(
            failure(failed("ERROR: disk full\n"), false),
            ExtractError::Failed("7-Zip failed: ERROR: disk full".into())
        );
        assert!(matches!(
            failure(RunError::TooMuchOutput(1), false),
            ExtractError::Unsupported(_)
        ));
    }
}
