//! Plugins as Settings manages them: on or off, updated from signed
//! releases, or the installed copy removed ([`PluginControl`]).
//!
//! Choices live in `plugins.json` beside the installed copies. An update is
//! checked ([`crate::release`]) and run once before it is used; any failure
//! keeps the copy in use. Replaced copies are deleted at the next start, and
//! only files the record names are ever deleted.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use wayfetch_engine::domain::name::PluginId;
use wayfetch_engine::media::{PluginControl, PluginError, PluginOutcome, PluginWork};
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

    /// Deletes the files the record lists for discarding, with the
    /// engine's [`PluginWork`] so it holds the state directory meanwhile.
    pub async fn sweep(&self, work: &PluginWork) -> Result<(), String> {
        let mut record = self.record.lock().await;
        if record.discard.is_empty() {
            return Ok(());
        }
        let in_use: BTreeSet<_> = record.installed.values().map(|i| i.file.clone()).collect();
        let (dir, discard) = (self.dir.clone(), record.discard.clone());
        let kept = work
            .blocking(move || {
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
        self.save(&record, work).await.map_err(|e| e.to_string())
    }

    /// Writes `record`.
    async fn save(&self, record: &Record, work: &PluginWork) -> Result<(), PluginError> {
        let bytes =
            serde_json::to_vec_pretty(record).map_err(|e| PluginError::Storage(e.to_string()))?;
        let dir = self.dir.clone();
        work.blocking(move || {
            std::fs::create_dir_all(&dir)?;
            replace_file(&dir.join(RECORD), &bytes)
        })
        .await?
        .map_err(|e| PluginError::Storage(format!("cannot save the plugin settings: {e}")))
    }

    async fn set_enabled(
        &self,
        plugin: &Plugin,
        enabled: bool,
        work: &PluginWork,
    ) -> Result<bool, PluginError> {
        let id = plugin.describe().id;
        let mut record = self.record.lock().await;
        let mut next = record.clone();
        let changed = if enabled {
            next.disabled.remove(id)
        } else {
            next.disabled.insert(id.to_owned())
        };
        if changed {
            self.save(&next, work).await?;
            *record = next;
            plugin.set_enabled(enabled);
        }
        Ok(changed)
    }

    async fn remove(&self, plugin: &Plugin, work: &PluginWork) -> Result<(), PluginError> {
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
        self.save(&next, work).await?;
        *record = next;
        plugin.set_installed(None);
        Ok(())
    }

    /// Installs the newest release of `plugin` if it is newer than the
    /// copy in use, or if the copy Wayfetch installed no longer runs;
    /// whether it did.
    async fn update(
        &self,
        plugin: &Plugin,
        release: &Release,
        work: &PluginWork,
    ) -> Result<bool, PluginError> {
        let describe = plugin.describe();
        let Some(transport) = self.transport.as_deref() else {
            return Err(PluginError::NotApplicable(
                "this Wayfetch cannot download plugins".into(),
            ));
        };
        // A copy Wayfetch installed that no longer runs is replaced, even
        // by the same release; the copy that works decides what is newer.
        let installed = plugin.installed();
        let repair = installed.as_ref().is_some_and(|p| !p.ready());
        let current = installed
            .filter(|p| p.ready())
            .unwrap_or_else(|| plugin.found());
        let current_version = current.ready().then(|| current.version()).flatten();
        // A repair never goes below the version last installed, which the
        // record keeps though its copy is gone.
        let recorded = match repair {
            true => self
                .record
                .lock()
                .await
                .installed
                .get(describe.id)
                .map(|i| i.version.clone()),
            false => None,
        };
        let asset = (release.asset)(current.path()).ok_or_else(|| {
            PluginError::NotApplicable(format!(
                "{} publishes no build for this system",
                describe.name
            ))
        })?;
        let (manifest, entries) = tokio::select! {
            fetched = release.manifest(transport) => fetched?,
            () = work.stopped() => return Err(stopping()),
        };
        let manifest = hex::encode(manifest);
        let expected = *entries.get(asset.name).ok_or_else(|| {
            PluginError::Rejected(format!("the release does not list {}", asset.name))
        })?;
        let sha256 = hex::encode(expected);
        if !repair {
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
        self.edit(
            |r| {
                r.discard.insert(file.clone());
                r.discard.insert(part.clone());
            },
            work,
        )
        .await?;
        let installed = self
            .install(
                transport, release, asset, &current, expected, &part_path, &file_path, work,
            )
            .await;
        // The part is never needed again.
        let _ = work.blocking(move || remove(&part_path)).await;
        let program = match installed {
            Ok(()) => Program::detect(describe, Some(file_path.clone()), plugin.check()).await,
            Err(e) => {
                self.forget(&file_path, &[&file, &part], work).await;
                return Err(e);
            }
        };
        let Some(version) = program.ready().then(|| program.version()).flatten() else {
            self.forget(&file_path, &[&file, &part], work).await;
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
        // Repairing accepts the version installed or working now, never
        // an older one.
        let stale = if repair {
            [current_version, recorded.as_deref()]
                .into_iter()
                .flatten()
                .any(|v| release::newer(v, &version))
        } else {
            current_version.is_some_and(|v| !release::newer(&version, v))
        };
        if stale {
            self.forget(&file_path, &[&file, &part], work).await;
            self.edit(
                |r| {
                    r.checked.insert(describe.id.into(), checked);
                },
                work,
            )
            .await?;
            if repair {
                return Err(PluginError::Rejected(format!(
                    "the newest {} is older than the copy installed before, so it is not used; remove the update to go back to the copy found",
                    describe.name
                )));
            }
            return Ok(false);
        }
        self.edit(
            |r| {
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
            },
            work,
        )
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
        work: &PluginWork,
    ) -> Result<(), PluginError> {
        let storage =
            |e: std::io::Error| PluginError::Storage(format!("cannot save the download: {e}"));
        let url = release.url(asset.name)?;
        // The file is written on a blocking thread that holds the state
        // directory until it has ended, whatever happens to the download.
        let (mut chunks, mut written) = tokio::sync::mpsc::channel::<bytes::Bytes>(8);
        let path = part.to_owned();
        let writer = work.blocking(move || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut out = std::fs::File::create(&path)?;
            while let Some(chunk) = written.blocking_recv() {
                std::io::Write::write_all(&mut out, &chunk)?;
            }
            out.sync_all()
        });
        let fetched = tokio::select! {
            fetched = release::fetch(transport, &url, release.max_bytes, &mut chunks) => fetched,
            () = work.stopped() => Err(stopping()),
        };
        drop(chunks);
        // A failed write says why; the download only saw the writer stop.
        writer.await?.map_err(storage)?;
        let digest = fetched?;
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
        work.blocking(move || -> std::io::Result<()> {
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
        .await?
        .map_err(storage)
    }

    /// Changes the record and saves it.
    async fn edit(
        &self,
        change: impl FnOnce(&mut Record),
        work: &PluginWork,
    ) -> Result<(), PluginError> {
        let mut record = self.record.lock().await;
        let mut next = record.clone();
        change(&mut next);
        self.save(&next, work).await?;
        *record = next;
        Ok(())
    }

    /// Deletes a new copy that will not be used, and drops `names` from
    /// the record once it is gone; if it cannot be deleted, the next
    /// start tries again.
    async fn forget(&self, file: &Path, names: &[&String], work: &PluginWork) {
        let file = file.to_owned();
        if let Ok(Ok(())) = work.blocking(move || remove(&file)).await {
            let _ = self
                .edit(
                    |r| {
                        for name in names {
                            r.discard.remove(*name);
                        }
                    },
                    work,
                )
                .await;
        }
    }
}

impl PluginControl for Plugins {
    fn change<'a>(
        &'a self,
        plugin: &'a PluginId,
        change: PluginChange,
        work: PluginWork,
    ) -> BoxFuture<'a, Result<PluginOutcome, PluginError>> {
        Box::pin(async move {
            let work = &work;
            let entry = self
                .entry(plugin.as_str())
                .ok_or_else(|| PluginError::Unknown(plugin.clone()))?;
            let describe = entry.plugin.describe();
            let changed = match change {
                PluginChange::Enable => self.set_enabled(&entry.plugin, true, work).await?,
                PluginChange::Disable => self.set_enabled(&entry.plugin, false, work).await?,
                PluginChange::Remove => self.remove(&entry.plugin, work).await.map(|()| true)?,
                PluginChange::Update => {
                    let release = entry.release.ok_or_else(|| {
                        PluginError::NotApplicable(format!(
                            "Wayfetch cannot update {}; it changes with Wayfetch or the system",
                            describe.name
                        ))
                    })?;
                    self.update(&entry.plugin, release, work).await?
                }
            };
            Ok(PluginOutcome {
                changed,
                version: entry.plugin.program().version().map(str::to_owned),
            })
        })
    }
}

/// Why a change stopped before it finished.
fn stopping() -> PluginError {
    PluginError::Unreachable("Wayfetch stopped the change; try again".into())
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

        let updated = plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await
            .unwrap();
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
        let again = plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await
            .unwrap();
        assert!(!again.changed);
        assert_eq!(releases.downloads(), 1);

        // The choice survives a restart.
        drop(plugins);
        let plugins = open(dir.path(), Some(found), &releases).await;
        let tool = plugins.get("tool").unwrap();
        assert_eq!(tool.program().version(), Some("2099.01.01"));

        // Removing goes back to the copy found; the file goes at the next
        // start, as a run may still use it.
        let removed = plugins
            .change(&id(), PluginChange::Remove, PluginWork::unowned())
            .await
            .unwrap();
        assert_eq!(removed.version.as_deref(), Some("2026.01.01"));
        assert!(!tool.summary(Vec::new()).installed);
        assert_eq!(files(dir.path()).len(), 1);
        plugins.sweep(&PluginWork::unowned()).await.unwrap();
        assert!(files(dir.path()).is_empty());
        let refused = plugins
            .change(&id(), PluginChange::Remove, PluginWork::unowned())
            .await;
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
        let refused = plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await;
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
        let unchanged = plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await
            .unwrap();
        assert_eq!(unchanged.version.as_deref(), Some("2100.01.01"));
        assert!(!unchanged.changed);
        assert!(files(dir.path()).is_empty());
        plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await
            .unwrap();
        assert_eq!(releases.downloads(), 1);
    }

    #[tokio::test]
    async fn a_plugin_turned_off_stays_off_and_a_missing_one_can_be_installed() {
        let dir = tempfile::tempdir().unwrap();
        let releases = Arc::new(Releases::default());
        let plugins = open(dir.path(), None, &releases).await;
        let tool = plugins.get("tool").unwrap();
        assert!(!tool.usable());
        let installed = plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await
            .unwrap();
        assert!(installed.changed && tool.usable());
        assert_eq!(tool.summary(Vec::new()).fallback_version, None);

        let off = plugins
            .change(&id(), PluginChange::Disable, PluginWork::unowned())
            .await
            .unwrap();
        assert!(off.changed && !tool.usable());
        assert!(
            !plugins
                .change(&id(), PluginChange::Disable, PluginWork::unowned())
                .await
                .unwrap()
                .changed
        );
        drop(plugins);
        let plugins = open(dir.path(), None, &releases).await;
        assert!(!plugins.get("tool").unwrap().enabled());
        let other = plugins
            .change(
                &PluginId::new("other").unwrap(),
                PluginChange::Enable,
                PluginWork::unowned(),
            )
            .await;
        assert!(matches!(other, Err(PluginError::Unknown(_))));
    }

    #[tokio::test]
    async fn an_update_replaces_an_installed_copy_that_went_missing() {
        let dir = tempfile::tempdir().unwrap();
        let releases = Arc::new(Releases::default());
        let plugins = open(dir.path(), None, &releases).await;
        let work = PluginWork::unowned;
        assert!(
            plugins
                .change(&id(), PluginChange::Update, work())
                .await
                .unwrap()
                .changed
        );
        drop(plugins);
        let installed = dir.path().join("plugins/tool").join(&files(dir.path())[0]);
        std::fs::remove_file(installed).unwrap();

        let plugins = open(dir.path(), None, &releases).await;
        let tool = plugins.get("tool").unwrap();
        assert!(!tool.usable());
        // The record names the same release, but the copy is gone.
        let repaired = plugins
            .change(&id(), PluginChange::Update, work())
            .await
            .unwrap();
        assert!(repaired.changed && tool.usable(), "{repaired:?}");
        assert_eq!(releases.downloads(), 2);
        assert_eq!(files(dir.path()), ["a65f5b61819a2df6-tool"]);
        plugins.sweep(&work()).await.unwrap();
        assert_eq!(
            files(dir.path()),
            ["a65f5b61819a2df6-tool"],
            "the copy in use stays"
        );
    }

    #[tokio::test]
    async fn a_repair_never_installs_a_release_older_than_the_one_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let plugins_dir = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        // Installed 2100.01.01 once; its copy has since gone.
        let record = Record {
            installed: BTreeMap::from([(
                "tool".to_owned(),
                Installed {
                    file: "tool/0000000000000000-tool".into(),
                    version: "2100.01.01".into(),
                    sha256: "00".repeat(32),
                },
            )]),
            ..Record::default()
        };
        std::fs::write(
            plugins_dir.join(RECORD),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let releases = Arc::new(Releases::default());
        let plugins = open(dir.path(), None, &releases).await;
        let tool = plugins.get("tool").unwrap();
        assert!(!tool.usable());

        // The signed release is 2099.01.01, older than the one recorded.
        let refused = plugins
            .change(&id(), PluginChange::Update, PluginWork::unowned())
            .await;
        assert!(
            matches!(&refused, Err(PluginError::Rejected(m)) if m.contains("older")),
            "{refused:?}"
        );
        assert!(!tool.usable());
        assert!(files(dir.path()).is_empty());
        let kept: Record =
            serde_json::from_slice(&std::fs::read(plugins_dir.join(RECORD)).unwrap()).unwrap();
        assert_eq!(kept.installed, record.installed);
        assert!(kept.discard.is_empty(), "{kept:?}");
    }
}
