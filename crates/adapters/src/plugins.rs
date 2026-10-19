//! The plugins as Settings changes them: turned on or off, updated from
//! their official signed releases, and copies Wayfetch installed removed
//! ([`PluginControl`], protocol 4.1).
//!
//! Choices are kept in `plugins.json` in the plugins directory of the
//! engine's state directory, replaced atomically on each change, and
//! installed copies sit beside it. An update downloads into a file the
//! record names before any byte arrives, checks the release's signature
//! and the file's checksum ([`crate::release`]), and runs the new copy
//! once; it is used only if it runs and reports a newer version, and
//! anything failing deletes it and leaves the copy in use (rollback). A
//! copy replaced or removed is deleted at the next start, since runs
//! that started with it may still read it; only files the record names
//! are ever deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use wayfetch_engine::domain::name::PluginId;
use wayfetch_engine::media::{PluginControl, PluginError, PluginOutcome};
use wayfetch_engine::protocol::media::PluginChange;
use wayfetch_engine::state_dir::replace_file;
use wayfetch_engine::transfer::transport::Transport;
use wayfetch_engine::transfer::writer::BoxFuture;

use crate::plugin::{Check, Describe, Plugin, Program};
use crate::release::{self, Release};

/// The record's file name.
const RECORD: &str = "plugins.json";

/// A plugin the host runs.
#[derive(Debug)]
pub struct Spec {
    /// What it is.
    pub describe: Describe,
    /// How its program is checked.
    pub check: Check,
    /// Where it was found, at a configured path or on `PATH`.
    pub found: Option<PathBuf>,
    /// Its official releases, if Settings may install them.
    pub release: Option<&'static Release>,
}

/// What Settings changed, as recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    /// Plugins turned off.
    #[serde(default)]
    disabled: BTreeSet<String>,
    /// Copies Wayfetch installed, by plugin.
    #[serde(default)]
    installed: BTreeMap<String, Installed>,
    /// Files Wayfetch wrote that nothing will use: relative paths in the
    /// plugins directory, deleted at the next start.
    #[serde(default)]
    discard: BTreeSet<String>,
    /// The newest release checked, by plugin, so checking again with the
    /// same manifest downloads nothing.
    #[serde(default)]
    checked: BTreeMap<String, Checked>,
}

/// A copy Wayfetch installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installed {
    /// Its relative path in the plugins directory.
    file: String,
    /// The version it reported.
    version: String,
    /// The SHA-256 digest of the release file, in hex.
    sha256: String,
}

/// A release checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checked {
    /// The SHA-256 digest of its signed manifest, in hex.
    manifest: String,
    /// The version its file reported.
    version: String,
}

/// One plugin and its releases.
struct Entry {
    plugin: Arc<Plugin>,
    release: Option<&'static Release>,
}

/// The plugins, and what Settings changed.
pub struct Plugins {
    dir: PathBuf,
    record: Mutex<Record>,
    entries: Vec<Entry>,
    transport: Option<Arc<dyn Transport>>,
}

impl std::fmt::Debug for Plugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugins")
            .field("dir", &self.dir)
            .field("plugins", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl Plugins {
    /// The plugins `specs` names, with the choices recorded in `dir`,
    /// each checked; releases are fetched through `transport`.
    pub async fn open(
        dir: PathBuf,
        specs: Vec<Spec>,
        transport: Option<Arc<dyn Transport>>,
    ) -> Result<Self, String> {
        let path = dir.join(RECORD);
        let record: Record = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let mut checks = tokio::task::JoinSet::new();
        for (i, spec) in specs.into_iter().enumerate() {
            let installed = record
                .installed
                .get(spec.describe.id)
                .and_then(|i| relative(&i.file))
                .map(|file| dir.join(file));
            checks.spawn(async move {
                let found = Program::detect(spec.describe, spec.found.clone(), &spec.check).await;
                let installed = match installed {
                    Some(path) => {
                        Some(Program::detect(spec.describe, Some(path), &spec.check).await)
                    }
                    None => None,
                };
                (i, spec, found, installed)
            });
        }
        let mut detected = checks.join_all().await;
        detected.sort_by_key(|(i, ..)| *i);
        let entries = detected
            .into_iter()
            .map(|(_, spec, found, installed)| {
                let plugin = Plugin::new(spec.describe, spec.check, found, spec.release.is_some());
                plugin.set_installed(installed);
                plugin.set_enabled(!record.disabled.contains(spec.describe.id));
                Entry {
                    plugin: Arc::new(plugin),
                    release: spec.release,
                }
            })
            .collect();
        Ok(Self {
            dir,
            record: Mutex::new(record),
            entries,
            transport,
        })
    }

    /// The plugin `id`.
    pub fn get(&self, id: &str) -> Option<Arc<Plugin>> {
        self.entry(id).map(|e| e.plugin.clone())
    }

    fn entry(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.plugin.describe().id == id)
    }

    /// Deletes the files the record lists for discarding. Call while the
    /// engine holds the state directory and before any plugin runs.
    pub async fn sweep(&self) -> Result<(), String> {
        let mut record = self.record.lock().await;
        if record.discard.is_empty() {
            return Ok(());
        }
        let in_use: BTreeSet<_> = record.installed.values().map(|i| i.file.clone()).collect();
        let (dir, discard) = (self.dir.clone(), record.discard.clone());
        let kept = tokio::task::spawn_blocking(move || {
            discard
                .into_iter()
                .filter(|file| {
                    in_use.contains(file)
                        || relative(file).is_some_and(|f| remove(&dir.join(f)).is_err())
                })
                .collect::<BTreeSet<_>>()
        })
        .await
        .map_err(|e| e.to_string())?;
        record.discard = kept;
        self.save(&record).await.map_err(|e| e.to_string())
    }

    /// Writes `record`.
    async fn save(&self, record: &Record) -> Result<(), PluginError> {
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| PluginError::Storage(e.to_string()))?;
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            replace_file(&dir.join(RECORD), &bytes)
        })
        .await
        .map_err(|e| PluginError::Storage(e.to_string()))?
        .map_err(|e| PluginError::Storage(format!("cannot save the plugin settings: {e}")))
    }

    async fn set_enabled(&self, plugin: &Plugin, enabled: bool) -> Result<bool, PluginError> {
        let id = plugin.describe().id;
        let mut record = self.record.lock().await;
        let mut next = record.clone();
        let changed = if enabled {
            next.disabled.remove(id)
        } else {
            next.disabled.insert(id.to_owned())
        };
        if changed {
            self.save(&next).await?;
            *record = next;
            plugin.set_enabled(enabled);
        }
        Ok(changed)
    }

    async fn remove(&self, plugin: &Plugin) -> Result<(), PluginError> {
        let describe = plugin.describe();
        let mut record = self.record.lock().await;
        let mut next = record.clone();
        let Some(installed) = next.installed.remove(describe.id) else {
            return Err(PluginError::NotApplicable(format!(
                "Wayfetch did not install this copy of {}, so it cannot remove it; turn it off instead",
                describe.name
            )));
        };
        next.discard.insert(installed.file);
        self.save(&next).await?;
        *record = next;
        plugin.set_installed(None);
        Ok(())
    }

    /// Installs the newest release of `plugin` if it is newer than the
    /// copy in use; whether it was.
    async fn update(&self, plugin: &Plugin, release: &Release) -> Result<bool, PluginError> {
        let describe = plugin.describe();
        let Some(transport) = self.transport.as_deref() else {
            return Err(PluginError::NotApplicable(
                "this Wayfetch cannot download plugins".into(),
            ));
        };
        let current = plugin.program();
        let current_version = current.ready().then(|| current.version()).flatten();
        let asset = (release.asset)(current.path()).ok_or_else(|| {
            PluginError::NotApplicable(format!(
                "{} publishes no build for this system",
                describe.name
            ))
        })?;
        let (manifest, entries) = release.manifest(transport).await?;
        let manifest = hex::encode(manifest);
        let expected = *entries.get(asset.name).ok_or_else(|| {
            PluginError::Rejected(format!("the release does not list {}", asset.name))
        })?;
        let sha256 = hex::encode(expected);
        {
            let record = self.record.lock().await;
            let same_file = record
                .installed
                .get(describe.id)
                .is_some_and(|i| i.sha256 == sha256);
            let seen = record.checked.get(describe.id).is_some_and(|c| {
                c.manifest == manifest
                    && current_version.is_some_and(|v| !release::newer(&c.version, v))
            });
            if same_file || seen {
                return Ok(false);
            }
        }
        let file = format!("{}/{}-{}", describe.id, &sha256[..16], asset.name);
        let part = format!("{file}.part");
        let (file_path, part_path) = (self.dir.join(&file), self.dir.join(&part));
        // Named before any byte is written, so a crash leaves nothing
        // the record does not name.
        self.edit(|r| {
            r.discard.insert(file.clone());
            r.discard.insert(part.clone());
        })
        .await?;
        let installed = self
            .install(
                transport, release, asset, &current, expected, &part_path, &file_path,
            )
            .await;
        // The part is never needed again.
        let _ = remove(&part_path);
        let program = match installed {
            Ok(()) => Program::detect(describe, Some(file_path.clone()), plugin.check()).await,
            Err(e) => {
                self.forget(&file_path, &[&file, &part]).await;
                return Err(e);
            }
        };
        let Some(version) = program.ready().then(|| program.version()).flatten() else {
            self.forget(&file_path, &[&file, &part]).await;
            let why = program.problem().unwrap_or("it reported no version");
            return Err(PluginError::Rejected(format!(
                "the new {} did not run, so the copy in use stays: {why}",
                describe.name
            )));
        };
        let version = version.to_owned();
        let checked = Checked {
            manifest,
            version: version.clone(),
        };
        if current_version.is_some_and(|v| !release::newer(&version, v)) {
            self.forget(&file_path, &[&file, &part]).await;
            self.edit(|r| {
                r.checked.insert(describe.id.into(), checked);
            })
            .await?;
            return Ok(false);
        }
        self.edit(|r| {
            if let Some(old) = r.installed.insert(
                describe.id.into(),
                Installed {
                    file: file.clone(),
                    version,
                    sha256,
                },
            ) {
                r.discard.insert(old.file);
            }
            r.discard.remove(&file);
            r.discard.remove(&part);
            r.checked.insert(describe.id.into(), checked);
        })
        .await?;
        plugin.set_installed(Some(program));
        Ok(true)
    }

    /// Downloads `asset` into `part`, checks it against `expected`, and
    /// moves it to `file`, runnable.
    #[allow(clippy::too_many_arguments)]
    async fn install(
        &self,
        transport: &dyn Transport,
        release: &Release,
        asset: release::Asset,
        current: &Program,
        expected: [u8; 32],
        part: &Path,
        file: &Path,
    ) -> Result<(), PluginError> {
        let storage =
            |e: std::io::Error| PluginError::Storage(format!("cannot save the download: {e}"));
        if let Some(dir) = part.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(storage)?;
        }
        let mut out = tokio::fs::File::create(part).await.map_err(storage)?;
        let digest = release::fetch(
            transport,
            &release.url(asset.name)?,
            release.max_bytes,
            &mut out,
        )
        .await?;
        drop(out);
        if digest != expected {
            return Err(PluginError::Rejected(format!(
                "the downloaded {} does not match its signed checksum",
                asset.name
            )));
        }
        let (part, file) = (part.to_owned(), file.to_owned());
        let interpreter = asset
            .zipapp
            .then(|| current.path().map(Path::to_owned))
            .flatten();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            match interpreter {
                Some(current) => {
                    let current = std::fs::read(current)?;
                    let zipapp = std::fs::read(&part)?;
                    let bytes = release::with_interpreter_of(&current, &zipapp)
                        .ok_or_else(|| std::io::Error::other("no interpreter line"))?;
                    std::fs::write(&file, bytes)?;
                }
                None => std::fs::rename(&part, &file)?,
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::File::open(&file)?.sync_all()
        })
        .await
        .map_err(|e| PluginError::Storage(e.to_string()))?
        .map_err(storage)
    }

    /// Changes the record and saves it.
    async fn edit(&self, change: impl FnOnce(&mut Record)) -> Result<(), PluginError> {
        let mut record = self.record.lock().await;
        let mut next = record.clone();
        change(&mut next);
        self.save(&next).await?;
        *record = next;
        Ok(())
    }

    /// Deletes a new copy that will not be used, and drops `names` from
    /// the record once it is gone; if it cannot be deleted, the next
    /// start tries again.
    async fn forget(&self, file: &Path, names: &[&String]) {
        if remove(file).is_ok() {
            let _ = self
                .edit(|r| {
                    for name in names {
                        r.discard.remove(*name);
                    }
                })
                .await;
        }
    }
}

impl PluginControl for Plugins {
    fn change<'a>(
        &'a self,
        plugin: &'a PluginId,
        change: PluginChange,
    ) -> BoxFuture<'a, Result<PluginOutcome, PluginError>> {
        Box::pin(async move {
            let entry = self
                .entry(plugin.as_str())
                .ok_or_else(|| PluginError::Unknown(plugin.clone()))?;
            let describe = entry.plugin.describe();
            let changed = match change {
                PluginChange::Enable => self.set_enabled(&entry.plugin, true).await?,
                PluginChange::Disable => self.set_enabled(&entry.plugin, false).await?,
                PluginChange::Remove => self.remove(&entry.plugin).await.map(|()| true)?,
                PluginChange::Update => {
                    let release = entry.release.ok_or_else(|| {
                        PluginError::NotApplicable(format!(
                            "Wayfetch cannot update {}; it changes with Wayfetch or the system",
                            describe.name
                        ))
                    })?;
                    self.update(&entry.plugin, release).await?
                }
            };
            Ok(PluginOutcome {
                changed,
                version: entry.plugin.program().version().map(str::to_owned),
            })
        })
    }
}

/// `file` as a path inside the plugins directory, if it is one.
fn relative(file: &str) -> Option<&Path> {
    let path = Path::new(file);
    path.components()
        .all(|c| matches!(c, Component::Normal(_)))
        .then_some(path)
}

/// Removes `path`, which may already be gone.
fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Mutex as StdMutex;

    use bytes::Bytes;
    use url::Url;
    use wayfetch_engine::protocol::media::PluginRole;
    use wayfetch_engine::transfer::lease::Representation;
    use wayfetch_engine::transfer::range::Framing;
    use wayfetch_engine::transfer::transport::{OpenRequest, Response, TransportError};
    use wayfetch_engine::transfer::writer::ChunkSource;

    use super::*;
    use crate::plugin::tests::{check, script};
    use crate::release::Asset;

    /// A release signed by a test key made with GnuPG for these tests
    /// only (its secret part was not kept): `tool` prints `2099.01.01`,
    /// and the signed manifest lists its SHA-256.
    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/signed-release");

    static TEST_RELEASE: Release = Release {
        base: "https://releases.example/latest/",
        manifest: "SHA2-256SUMS",
        signature: "SHA2-256SUMS.sig",
        key: include_str!("../tests/fixtures/signed-release/key.asc"),
        fingerprint: "B636F75CE951E2DE0D8BF7D835993F04CCC156F1",
        asset: |_| {
            Some(Asset {
                name: "tool",
                zipapp: false,
            })
        },
        max_bytes: 1024,
    };

    const DESCRIBE: Describe = Describe {
        id: "tool",
        name: "Tool",
        role: PluginRole::MediaResolver,
    };

    /// Serves the fixture release, with `tool` replaced if set, and
    /// counts the requests for each file.
    #[derive(Default)]
    struct Releases {
        tool: StdMutex<Option<Vec<u8>>>,
        requests: StdMutex<Vec<String>>,
    }

    struct Once(Option<Bytes>);

    impl ChunkSource for Once {
        fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
            Box::pin(async move { Ok(self.0.take()) })
        }
    }

    impl Transport for Releases {
        fn open<'a>(
            &'a self,
            request: &'a OpenRequest,
        ) -> BoxFuture<'a, Result<Response, TransportError>> {
            Box::pin(async move {
                let name = request.url.path().rsplit('/').next().unwrap().to_owned();
                self.requests.lock().unwrap().push(name.clone());
                let data = match (name.as_str(), self.tool.lock().unwrap().clone()) {
                    ("tool", Some(tool)) => tool,
                    _ => std::fs::read(Path::new(FIXTURE).join(&name)).unwrap(),
                };
                Ok(Response {
                    framing: Framing::Full {
                        length: Some(data.len() as u64),
                    },
                    representation: Representation::default(),
                    filename: None,
                    body: Box::new(Once(Some(data.into()))),
                })
            })
        }
    }

    impl Releases {
        fn downloads(&self) -> usize {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|n| *n == "tool")
                .count()
        }
    }

    async fn open(dir: &Path, found: Option<PathBuf>, releases: &Arc<Releases>) -> Plugins {
        let spec = Spec {
            describe: DESCRIBE,
            check: check(),
            found,
            release: Some(&TEST_RELEASE),
        };
        Plugins::open(dir.join("plugins"), vec![spec], Some(releases.clone()))
            .await
            .unwrap()
    }

    fn id() -> PluginId {
        PluginId::new("tool").unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir.join("plugins/tool"))
            .map(|d| {
                d.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn the_test_release_is_signed_by_its_key() {
        let manifest = std::fs::read(Path::new(FIXTURE).join("SHA2-256SUMS")).unwrap();
        let signature = std::fs::read(Path::new(FIXTURE).join("SHA2-256SUMS.sig")).unwrap();
        assert!(
            TEST_RELEASE
                .verify(&manifest, &signature)
                .unwrap()
                .contains_key("tool")
        );
        assert_eq!(
            TEST_RELEASE.url("tool").unwrap(),
            Url::parse("https://releases.example/latest/tool").unwrap()
        );
    }

    #[tokio::test]
    async fn an_update_installs_a_verified_newer_release_and_remove_goes_back() {
        let dir = tempfile::tempdir().unwrap();
        let found = script(dir.path(), "found", "echo 2026.01.01");
        let releases = Arc::new(Releases::default());
        let plugins = open(dir.path(), Some(found.clone()), &releases).await;
        let tool = plugins.get("tool").unwrap();

        let updated = plugins.change(&id(), PluginChange::Update).await.unwrap();
        assert_eq!(
            updated,
            PluginOutcome {
                changed: true,
                version: Some("2099.01.01".into())
            }
        );
        let summary = tool.summary(Vec::new());
        assert!(summary.installed);
        assert_eq!(summary.fallback_version.as_deref(), Some("2026.01.01"));
        assert_eq!(files(dir.path()), ["a65f5b61819a2df6-tool"]);

        // The same release again downloads nothing.
        let again = plugins.change(&id(), PluginChange::Update).await.unwrap();
        assert!(!again.changed);
        assert_eq!(releases.downloads(), 1);

        // The choice survives a restart.
        drop(plugins);
        let plugins = open(dir.path(), Some(found), &releases).await;
        let tool = plugins.get("tool").unwrap();
        assert_eq!(tool.program().version(), Some("2099.01.01"));

        // Removing goes back to the copy found; the file goes at the next
        // start, as a run may still use it.
        let removed = plugins.change(&id(), PluginChange::Remove).await.unwrap();
        assert_eq!(removed.version.as_deref(), Some("2026.01.01"));
        assert!(!tool.summary(Vec::new()).installed);
        assert_eq!(files(dir.path()).len(), 1);
        plugins.sweep().await.unwrap();
        assert!(files(dir.path()).is_empty());
        let refused = plugins.change(&id(), PluginChange::Remove).await;
        assert!(matches!(refused, Err(PluginError::NotApplicable(_))));
    }

    #[tokio::test]
    async fn a_release_failing_its_checksum_or_not_newer_leaves_the_copy_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let releases = Arc::new(Releases::default());
        *releases.tool.lock().unwrap() = Some(b"#!/bin/sh\necho 2099.01.02\n".to_vec());
        let found = script(dir.path(), "found", "echo 2026.01.01");
        let plugins = open(dir.path(), Some(found), &releases).await;
        let tool = plugins.get("tool").unwrap();
        let refused = plugins.change(&id(), PluginChange::Update).await;
        assert!(matches!(refused, Err(PluginError::Rejected(m)) if m.contains("signed checksum")));
        assert_eq!(tool.program().version(), Some("2026.01.01"));
        assert!(files(dir.path()).is_empty());
        let record: Record = serde_json::from_slice(
            &std::fs::read(dir.path().join("plugins").join(RECORD)).unwrap(),
        )
        .unwrap();
        assert!(
            record.discard.is_empty() && record.installed.is_empty(),
            "{record:?}"
        );

        // A copy newer than the release stays, and the checked release is
        // not downloaded again.
        let dir = tempfile::tempdir().unwrap();
        let releases = Arc::new(Releases::default());
        let found = script(dir.path(), "found", "echo 2100.01.01");
        let plugins = open(dir.path(), Some(found), &releases).await;
        let unchanged = plugins.change(&id(), PluginChange::Update).await.unwrap();
        assert_eq!(unchanged.version.as_deref(), Some("2100.01.01"));
        assert!(!unchanged.changed);
        assert!(files(dir.path()).is_empty());
        plugins.change(&id(), PluginChange::Update).await.unwrap();
        assert_eq!(releases.downloads(), 1);
    }

    #[tokio::test]
    async fn a_plugin_turned_off_stays_off_and_a_missing_one_can_be_installed() {
        let dir = tempfile::tempdir().unwrap();
        let releases = Arc::new(Releases::default());
        let plugins = open(dir.path(), None, &releases).await;
        let tool = plugins.get("tool").unwrap();
        assert!(!tool.usable());
        let installed = plugins.change(&id(), PluginChange::Update).await.unwrap();
        assert!(installed.changed && tool.usable());
        assert_eq!(tool.summary(Vec::new()).fallback_version, None);

        let off = plugins.change(&id(), PluginChange::Disable).await.unwrap();
        assert!(off.changed && !tool.usable());
        assert!(
            !plugins
                .change(&id(), PluginChange::Disable)
                .await
                .unwrap()
                .changed
        );
        drop(plugins);
        let plugins = open(dir.path(), None, &releases).await;
        assert!(!plugins.get("tool").unwrap().enabled());
        let other = plugins
            .change(&PluginId::new("other").unwrap(), PluginChange::Enable)
            .await;
        assert!(matches!(other, Err(PluginError::Unknown(_))));
    }
}
