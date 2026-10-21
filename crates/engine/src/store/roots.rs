//! Destination roots by path, and the user's settings (ADR-17, ADR-29).
//!
//! A root's ID is allocated here and written into its sentinel, so the ID
//! a job records keeps naming the same directory across restarts and
//! changes to the operator's configuration.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::RootId;
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;
use wayfetch_protocol::command::{PreferenceChange, SpeedLimitChange};
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::query::SpeedLimit;
use wayfetch_protocol::{CommandResult, ErrorCode, ProtocolError};

use super::{OrCorrupt, Store, StoreError, key, outbox, sql};

/// A recorded root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootRow {
    pub(crate) id: RootId,
    pub(crate) path: PathBuf,
    /// Added by the host on the user's request, so permitted whether or
    /// not the operator configures it.
    pub(crate) registered: bool,
    /// Removed by the user, so not permitted even while the operator
    /// configures it, until the user adds it again.
    pub(crate) removed: bool,
}

/// A path's stored form. Roots must have UTF-8 paths, so a client can be
/// shown exactly where one is.
pub(crate) fn path_text(path: &Path) -> Result<&str, StoreError> {
    path.to_str()
        .ok_or(StoreError::Root("path is not valid UTF-8"))
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, bool, bool)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn decode(
    (id, path, registered, removed): (i64, String, bool, bool),
) -> Result<RootRow, StoreError> {
    Ok(RootRow {
        id: key(id)?,
        path: path.into(),
        registered,
        removed,
    })
}

impl Store {
    /// Every recorded root, in ID order.
    pub(crate) fn root_rows(&mut self) -> Result<Vec<RootRow>, StoreError> {
        let tx = self.read()?;
        let rows = tx
            .prepare("SELECT id, path, registered, removed FROM roots ORDER BY id")?
            .query_map([], read_row)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode).collect()
    }

    /// The root recorded at `path`.
    pub(crate) fn root_at(&mut self, path: &Path) -> Result<Option<RootRow>, StoreError> {
        let path = path_text(path)?;
        let tx = self.read()?;
        tx.query_row(
            "SELECT id, path, registered, removed FROM roots WHERE path = ?1",
            [path],
            read_row,
        )
        .optional()?
        .map(decode)
        .transpose()
    }

    /// Whether the root at `path` is recorded and finished initializing.
    pub(crate) fn root_initialized(&mut self, path: &Path) -> Result<bool, StoreError> {
        let path = path_text(path)?;
        let tx = self.read()?;
        Ok(tx
            .query_row(
                "SELECT initialized FROM roots WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Records the root a sentinel names, found at `path`: `initialized`
    /// unless an initialization is still to finish it. Refused if that ID
    /// is recorded at another path: the root moved, or a sentinel was
    /// copied, and either way the operator decides.
    pub(crate) fn adopt_root(
        &mut self,
        id: RootId,
        path: &Path,
        initialized: bool,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let text = path_text(path)?;
        let tx = self.write()?;
        let recorded: Option<String> = tx
            .query_row("SELECT path FROM roots WHERE id = ?1", [sql(id)?], |row| {
                row.get(0)
            })
            .optional()?;
        match recorded {
            Some(recorded) if recorded == text => {}
            Some(recorded) => return Err(StoreError::RootMoved { id, recorded }),
            None => {
                tx.execute(
                    "INSERT INTO roots (id, path, registered, initialized, created_at) \
                     VALUES (?1, ?2, 0, ?3, ?4)",
                    params![sql(id)?, text, initialized, now.unix_millis()],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A new root ID for `path`, before its sentinel is written.
    pub(crate) fn allocate_root(
        &mut self,
        path: &Path,
        registered: bool,
        now: UtcTimestamp,
    ) -> Result<RootId, StoreError> {
        let text = path_text(path)?;
        let tx = self.write()?;
        tx.execute(
            "INSERT INTO roots (path, registered, initialized, created_at) \
             VALUES (?1, ?2, 0, ?3)",
            params![text, registered, now.unix_millis()],
        )?;
        let id = key(tx.last_insert_rowid())?;
        tx.commit()?;
        Ok(id)
    }

    /// Marks root `id` as initialized: its marker and folders are written.
    pub(crate) fn root_was_initialized(&mut self, id: RootId) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute("UPDATE roots SET initialized = 1 WHERE id = ?1", [sql(id)?])?;
        tx.commit()?;
        Ok(())
    }

    /// Marks root `id` as registered on the user's request, which also
    /// restores a root they removed.
    pub(crate) fn register_root(&mut self, id: RootId) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE roots SET registered = 1, removed = 0 WHERE id = ?1",
            [sql(id)?],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Forgets a root allocated for a directory that could not be
    /// initialized. Nothing can refer to it yet: the coordinator learns of
    /// a root only once its sentinel is written.
    pub(crate) fn forget_root(&mut self, id: RootId) -> Result<(), StoreError> {
        let tx = self.write()?;
        tx.execute("DELETE FROM roots WHERE id = ?1", [sql(id)?])?;
        tx.commit()?;
        Ok(())
    }

    /// The user's settings as saved.
    pub(crate) fn settings(&mut self) -> Result<SavedSettings, StoreError> {
        let tx = self.read()?;
        settings(&tx)
    }

    /// Whether a command is recorded under `idempotency_key`, so running
    /// it again only replays its result.
    pub(crate) fn has_command(&mut self, idempotency_key: &str) -> Result<bool, StoreError> {
        let tx = self.read()?;
        Ok(tx
            .query_row(
                "SELECT 1 FROM commands WHERE idempotency_key = ?1",
                [idempotency_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }
}

/// The user's settings as saved: the default destination they chose and
/// their engine preferences, each `None` until chosen. The operator's
/// policy decides what applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SavedSettings {
    /// Revision to present with the next change.
    pub(crate) revision: Revision,
    /// The chosen default root, which may no longer be permitted.
    pub(crate) default_root: Option<RootId>,
    /// The saved most downloads at once, which may no longer be permitted.
    pub(crate) max_active_downloads: Option<u64>,
    /// The saved total download speed limit, which may no longer be
    /// permitted.
    pub(crate) download_speed_limit: Option<SpeedLimit>,
    /// Whether downloaded archives are unpacked as they finish.
    pub(crate) extract_archives: bool,
    /// Whether the engine checks for a newer release on its own.
    pub(crate) automatic_update_checks: bool,
}

/// A speed limit's stored form: 0 is Unlimited.
fn stored_speed_limit(limit: SpeedLimit) -> Result<i64, StoreError> {
    match limit {
        SpeedLimit::Unlimited => Ok(0),
        SpeedLimit::BytesPerSecond(0) => Err(StoreError::Corrupt("download_speed_limit")),
        SpeedLimit::BytesPerSecond(rate) => i64::try_from(rate).or_corrupt("download_speed_limit"),
    }
}

fn read_speed_limit(stored: i64) -> Result<SpeedLimit, StoreError> {
    match u64::try_from(stored) {
        Ok(0) => Ok(SpeedLimit::Unlimited),
        Ok(rate) => Ok(SpeedLimit::BytesPerSecond(rate)),
        Err(_) => Err(StoreError::Corrupt("download_speed_limit")),
    }
}

fn settings(tx: &Transaction<'_>) -> Result<SavedSettings, StoreError> {
    let (revision, root, max_active, speed, extract, updates): (
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        bool,
        bool,
    ) = tx.query_row(
        "SELECT revision, default_root, max_active_downloads, download_speed_limit, \
         extract_archives, automatic_update_checks FROM settings WHERE id = 1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    Ok(SavedSettings {
        revision: key(revision)?,
        default_root: root.map(key).transpose()?,
        max_active_downloads: max_active
            .map(|v| u64::try_from(v).or_corrupt("max_active_downloads"))
            .transpose()?,
        download_speed_limit: speed.map(read_speed_limit).transpose()?,
        extract_archives: extract,
        automatic_update_checks: updates,
    })
}

/// Refused unless the client saw the current settings revision.
fn check_revision(saved: &SavedSettings, expected: Revision) -> Result<(), ProtocolError> {
    if expected == saved.revision {
        return Ok(());
    }
    Err(ProtocolError {
        current_revision: Some(saved.revision),
        ..ProtocolError::new(
            ErrorCode::RevisionConflict,
            "settings changed since they were read",
        )
    })
}

/// The next settings revision, announced to clients in the same
/// transaction as the change.
fn next_revision(
    tx: &Transaction<'_>,
    saved: &SavedSettings,
    now: UtcTimestamp,
) -> Result<Revision, ProtocolError> {
    let next = saved.revision.next().or_corrupt("settings revision")?;
    outbox::append(tx, &EventKind::SettingsChanged { revision: next }, now)?;
    Ok(next)
}

impl Store {
    /// Records a plugin change the host made, under the command's key. A
    /// change that changed something moves the settings revision with a
    /// `settings_changed` event, puts media links whose check failed back
    /// to be checked (the plugin may work now), and announces
    /// `links_changed`, since the choices links offer may have changed. A
    /// retry of a recorded command only replays its result.
    pub(crate) fn record_plugin_change(
        &mut self,
        envelope: &wayfetch_protocol::CommandEnvelope,
        instance: wayfetch_protocol::InstanceId,
        result: &CommandResult,
        changed: bool,
        now: UtcTimestamp,
    ) -> Result<CommandResult, ProtocolError> {
        let tx = self.write()?;
        if let Some(result) = super::commands::replayed(&tx, envelope, instance)? {
            return Ok(result);
        }
        if changed {
            let next = next_revision(&tx, &settings(&tx)?, now)?;
            tx.execute(
                "UPDATE settings SET revision = ?1, updated_at = ?2 WHERE id = 1",
                params![sql(next)?, now.unix_millis()],
            )
            .map_err(StoreError::from)?;
            super::media::retry_failed(&tx, now)?;
            outbox::append(&tx, &EventKind::LinksChanged, now)?;
        }
        super::commands::record(&tx, envelope, result, now)?;
        tx.commit().map_err(StoreError::from)?;
        Ok(result.clone())
    }
}

/// Changes the settings at the revision the client saw, all or none: the
/// default root if given, and each preference given. The coordinator has
/// checked that the root is permitted and available and that each value
/// is within the operator's policy. A saved preference stays saved even
/// when it equals the default.
#[allow(clippy::too_many_arguments)]
pub(super) fn update_settings(
    tx: &Transaction<'_>,
    expected: Revision,
    default_root: Option<RootId>,
    max_active_downloads: Option<PreferenceChange>,
    download_speed_limit: Option<SpeedLimitChange>,
    extract_archives: Option<bool>,
    automatic_update_checks: Option<bool>,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let saved = settings(tx)?;
    check_revision(&saved, expected)?;
    let wanted = SavedSettings {
        default_root: default_root.or(saved.default_root),
        max_active_downloads: match max_active_downloads {
            None => saved.max_active_downloads,
            Some(PreferenceChange::Set(value)) => Some(value),
            Some(PreferenceChange::UseDefault) => None,
        },
        download_speed_limit: match download_speed_limit {
            None => saved.download_speed_limit,
            Some(SpeedLimitChange::Set(limit)) => Some(limit),
            Some(SpeedLimitChange::UseDefault) => None,
        },
        extract_archives: extract_archives.unwrap_or(saved.extract_archives),
        automatic_update_checks: automatic_update_checks.unwrap_or(saved.automatic_update_checks),
        ..saved
    };
    if wanted == saved {
        return Ok(CommandResult::SettingsUpdated {
            revision: saved.revision,
            changed: false,
        });
    }
    let next = next_revision(tx, &saved, now)?;
    let max_active = wanted
        .max_active_downloads
        .map(|v| i64::try_from(v).or_corrupt("max_active_downloads"))
        .transpose()?;
    let speed = wanted
        .download_speed_limit
        .map(stored_speed_limit)
        .transpose()?;
    tx.execute(
        "UPDATE settings SET revision = ?1, default_root = ?2, max_active_downloads = ?3, \
         download_speed_limit = ?4, extract_archives = ?5, automatic_update_checks = ?6, \
         updated_at = ?7 WHERE id = 1",
        params![
            sql(next)?,
            wanted.default_root.map(sql).transpose()?,
            max_active,
            speed,
            wanted.extract_archives,
            wanted.automatic_update_checks,
            now.unix_millis()
        ],
    )
    .map_err(StoreError::from)?;
    Ok(CommandResult::SettingsUpdated {
        revision: next,
        changed: true,
    })
}

/// Stops offering root `id` at the settings revision the client saw,
/// whether the user added it or the operator configures it: it stays
/// recorded, marked removed, so it stays out while configured and adding
/// its folder again restores the same root. The coordinator has checked
/// that it is permitted. The default, saved or in effect (`default`), and
/// a root that unfinished jobs save to, are refused; jobs submitted
/// without a destination save to the operator's `download_root`.
pub(super) fn remove_root(
    tx: &Transaction<'_>,
    expected: Revision,
    id: RootId,
    default: Option<RootId>,
    download_root: Option<RootId>,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let saved = settings(tx)?;
    check_revision(&saved, expected)?;
    let root = sql(id)?;
    let removed: Option<bool> = tx
        .query_row("SELECT removed FROM roots WHERE id = ?1", [root], |row| {
            row.get(0)
        })
        .optional()
        .map_err(StoreError::from)?;
    if removed != Some(false) {
        return Err(ProtocolError::new(
            ErrorCode::NotFound,
            "destination not found",
        ));
    }
    if saved.default_root == Some(id) || default == Some(id) {
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            "new downloads are saved to this folder; make another folder the default first",
        ));
    }
    let unfinished: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM jobs \
             WHERE (root_id = ?1 OR (?2 AND root_id IS NULL)) \
             AND stage NOT IN ('complete', 'failed', 'cancelled')",
            params![root, download_root == Some(id)],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    if unfinished > 0 {
        let noun = if unfinished == 1 {
            "download is"
        } else {
            "downloads are"
        };
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            &format!(
                "{unfinished} unfinished {noun} saving to this folder; let them finish or cancel them first"
            ),
        ));
    }
    let next = next_revision(tx, &saved, now)?;
    tx.execute(
        "UPDATE roots SET registered = 0, removed = 1 WHERE id = ?1",
        [root],
    )
    .map_err(StoreError::from)?;
    tx.execute(
        "UPDATE settings SET revision = ?1, updated_at = ?2 WHERE id = 1",
        params![sql(next)?, now.unix_millis()],
    )
    .map_err(StoreError::from)?;
    Ok(CommandResult::SettingsUpdated {
        revision: next,
        changed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    fn now() -> UtcTimestamp {
        UtcTimestamp::from_unix_millis(1)
    }

    #[test]
    fn new_roots_never_take_the_legacy_id() {
        let (_dir, mut store) = store();
        let id = store.allocate_root(Path::new("/a"), false, now()).unwrap();
        assert_eq!(id.get(), 2);
        // The root earlier versions marked as 1 is recorded from its
        // sentinel, and later IDs continue past it.
        store
            .adopt_root(
                RootId::from_u64(1).unwrap(),
                Path::new("/legacy"),
                true,
                now(),
            )
            .unwrap();
        store
            .adopt_root(RootId::from_u64(9).unwrap(), Path::new("/b"), true, now())
            .unwrap();
        let next = store.allocate_root(Path::new("/c"), true, now()).unwrap();
        assert_eq!(next.get(), 10);
        let rows = store.root_rows().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.get()).collect::<Vec<_>>(),
            [1, 2, 9, 10]
        );
        assert!(rows[3].registered && !rows[0].registered);
        assert_eq!(store.root_at(Path::new("/b")).unwrap().unwrap().id.get(), 9);
    }

    #[test]
    fn a_sentinel_recorded_elsewhere_is_refused() {
        let (_dir, mut store) = store();
        let id = store.allocate_root(Path::new("/a"), false, now()).unwrap();
        store.adopt_root(id, Path::new("/a"), true, now()).unwrap();
        assert!(matches!(
            store.adopt_root(id, Path::new("/copy"), true, now()),
            Err(StoreError::RootMoved { recorded, .. }) if recorded == "/a"
        ));
        // One path holds one root.
        assert!(store.allocate_root(Path::new("/a"), false, now()).is_err());
    }

    #[test]
    fn settings_are_revision_checked() {
        let (_dir, mut store) = store();
        let root = store.allocate_root(Path::new("/a"), false, now()).unwrap();
        let first = store.settings().unwrap();
        assert_eq!(
            (
                first.revision.get(),
                first.default_root,
                first.max_active_downloads
            ),
            (1, None, None)
        );
        let first = first.revision;
        let tx = store.write().unwrap();
        let updated =
            update_settings(&tx, first, Some(root), None, None, None, None, now()).unwrap();
        assert_eq!(
            updated,
            CommandResult::SettingsUpdated {
                revision: first.next().unwrap(),
                changed: true
            }
        );
        let stale =
            update_settings(&tx, first, Some(root), None, None, None, None, now()).unwrap_err();
        assert_eq!(stale.code, ErrorCode::RevisionConflict);
        assert_eq!(stale.current_revision, first.next().ok());
        let same = update_settings(
            &tx,
            first.next().unwrap(),
            Some(root),
            None,
            None,
            None,
            None,
            now(),
        )
        .unwrap();
        assert_eq!(
            same,
            CommandResult::SettingsUpdated {
                revision: first.next().unwrap(),
                changed: false
            }
        );
        tx.commit().unwrap();
        assert_eq!(store.settings().unwrap().default_root, Some(root));
        // One event, for the one change.
        assert_eq!(
            store.events_after(0, 10).unwrap(),
            [(
                1,
                EventKind::SettingsChanged {
                    revision: first.next().unwrap()
                }
            )]
        );
    }

    #[test]
    fn a_saved_preference_stays_saved_until_the_default_is_chosen() {
        let (_dir, mut store) = store();
        let root = store.allocate_root(Path::new("/a"), false, now()).unwrap();
        let tx = store.write().unwrap();
        let first = settings(&tx).unwrap().revision;
        update_settings(&tx, first, Some(root), None, None, None, None, now()).unwrap();
        let change = |revision: Revision, change| match update_settings(
            &tx,
            revision,
            None,
            Some(change),
            None,
            None,
            None,
            now(),
        )
        .unwrap()
        {
            CommandResult::SettingsUpdated { revision, changed } => (revision, changed),
            other => panic!("{other:?}"),
        };
        let second = first.next().unwrap();
        // Saving the built-in value is still a saved choice.
        let (third, changed) = change(second, PreferenceChange::Set(4));
        assert!(changed);
        let saved = settings(&tx).unwrap();
        assert_eq!(saved.max_active_downloads, Some(4));
        // Another setting is untouched by a preference change.
        assert_eq!(saved.default_root, Some(root));
        assert_eq!(change(third, PreferenceChange::Set(4)), (third, false));
        let (fourth, changed) = change(third, PreferenceChange::UseDefault);
        assert!(changed);
        assert_eq!(settings(&tx).unwrap().max_active_downloads, None);
        assert_eq!(
            change(fourth, PreferenceChange::UseDefault),
            (fourth, false)
        );
    }

    #[test]
    fn a_saved_unlimited_differs_from_no_saved_speed_limit() {
        let (_dir, mut store) = store();
        let tx = store.write().unwrap();
        let first = settings(&tx).unwrap().revision;
        let change = |revision: Revision, max_active, speed| match update_settings(
            &tx,
            revision,
            None,
            max_active,
            speed,
            None,
            None,
            now(),
        )
        .unwrap()
        {
            CommandResult::SettingsUpdated { revision, changed } => (revision, changed),
            other => panic!("{other:?}"),
        };
        let unlimited = Some(SpeedLimitChange::Set(SpeedLimit::Unlimited));
        let (second, changed) = change(first, None, unlimited);
        assert!(changed);
        assert_eq!(
            settings(&tx).unwrap().download_speed_limit,
            Some(SpeedLimit::Unlimited)
        );
        // Both preferences change together, in one revision.
        let rate = SpeedLimit::BytesPerSecond(2 << 20);
        let (third, changed) = change(
            second,
            Some(PreferenceChange::Set(3)),
            Some(SpeedLimitChange::Set(rate)),
        );
        assert_eq!(third, second.next().unwrap());
        assert!(changed);
        let saved = settings(&tx).unwrap();
        assert_eq!(
            (saved.max_active_downloads, saved.download_speed_limit),
            (Some(3), Some(rate))
        );
        let (_, changed) = change(third, None, Some(SpeedLimitChange::UseDefault));
        assert!(changed);
        let saved = settings(&tx).unwrap();
        assert_eq!(
            (saved.max_active_downloads, saved.download_speed_limit),
            (Some(3), None)
        );
        // Turning automatic updates off is one more setting.
        assert!(settings(&tx).unwrap().automatic_update_checks);
        let current = settings(&tx).unwrap().revision;
        update_settings(&tx, current, None, None, None, None, Some(false), now()).unwrap();
        assert!(!settings(&tx).unwrap().automatic_update_checks);
        // A stale revision changes neither.
        let stale =
            update_settings(&tx, second, None, None, unlimited, None, None, now()).unwrap_err();
        assert_eq!(stale.code, ErrorCode::RevisionConflict);
        assert_eq!(settings(&tx).unwrap().download_speed_limit, None);
    }
}
