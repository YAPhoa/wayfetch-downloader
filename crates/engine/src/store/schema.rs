//! Schema migrations.
//!
//! Policy:
//!
//! * Migrations are numbered SQL files under `store/migrations/`, applied in
//!   order with [`rusqlite_migration`], which records progress in SQLite's
//!   `user_version`. Each runs in its own transaction.
//! * They are forward-only and append-only once merged: a merged migration is
//!   never edited, because existing state directories have already applied
//!   it. A change is a new file.
//! * An engine refuses to open a database whose version is newer than it
//!   knows ([`super::StoreError::SchemaTooNew`]), so a downgrade cannot write
//!   through a schema it does not understand. Rolling back a release means
//!   restoring a matching backup of the state directory.
//! * A migration must keep stored identifiers, revisions, generations, event
//!   positions, and idempotency records valid.

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations, SchemaVersion};

use super::StoreError;

const MIGRATIONS: &[M<'static>] = &[
    M::up(include_str!("migrations/0001_initial.sql")),
    M::up(include_str!("migrations/0002_staging_publication.sql")),
    M::up(include_str!("migrations/0003_transfer_retention.sql")),
    M::up(include_str!("migrations/0004_representation_resource.sql")),
    M::up(include_str!("migrations/0005_staging_cleanup.sql")),
    M::up(include_str!("migrations/0006_destinations.sql")),
    M::up(include_str!("migrations/0007_job_mutations.sql")),
    M::up(include_str!("migrations/0008_collected_links.sql")),
    M::up(include_str!("migrations/0009_manual_retry.sql")),
    M::up(include_str!("migrations/0010_preferences.sql")),
    M::up(include_str!("migrations/0011_accounts.sql")),
    M::up(include_str!("migrations/0012_provider_downloads.sql")),
    M::up(include_str!("migrations/0013_speed_limit.sql")),
    M::up(include_str!("migrations/0014_download_removal.sql")),
    M::up(include_str!("migrations/0015_job_names.sql")),
    M::up(include_str!("migrations/0016_media_links.sql")),
    M::up(include_str!("migrations/0018_link_checks.sql")),
];

/// Newest schema version this engine knows.
pub const LATEST: usize = MIGRATIONS.len();

/// Brings the database to [`LATEST`] and returns that version.
pub(super) fn migrate(conn: &mut Connection) -> Result<usize, StoreError> {
    let migrations = Migrations::from_slice(MIGRATIONS);
    if let SchemaVersion::Outside(found) = migrations.current_version(conn)? {
        return Err(StoreError::SchemaTooNew {
            found: found.get(),
            supported: LATEST,
        });
    }
    migrations.to_latest(conn)?;
    Ok(LATEST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn migrations_are_valid() {
        Migrations::from_slice(MIGRATIONS).validate().unwrap();
    }

    #[test]
    fn reopening_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.info().schema_version, LATEST);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        drop(Store::open(dir.path()).unwrap());
        let conn = Connection::open(dir.path().join(crate::store::DATABASE_FILE)).unwrap();
        conn.pragma_update(None, "user_version", i64::try_from(LATEST + 1).unwrap())
            .unwrap();
        drop(conn);
        assert!(matches!(
            Store::open(dir.path()),
            Err(StoreError::SchemaTooNew { found, supported })
                if found == LATEST + 1 && supported == LATEST
        ));
    }
    #[test]
    fn accounts_upgrade_preserves_existing_preferences() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATIONS[..10])
            .to_latest(&mut conn)
            .unwrap();
        conn.execute(
            "UPDATE settings SET revision = 7, max_active_downloads = 3 WHERE id = 1",
            [],
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        let saved: (i64, i64) = conn
            .query_row(
                "SELECT revision, max_active_downloads FROM settings WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(saved, (7, 3));
        let accounts: i64 = conn
            .query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(accounts, 0);
    }

    #[test]
    fn name_upgrade_preserves_existing_job_identity_and_source() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATIONS[..14])
            .to_latest(&mut conn)
            .unwrap();
        conn.execute_batch(
            "INSERT INTO packages (id, name, created_at) VALUES (1, 'p', 0);
             INSERT INTO input_batches (id, package_id, input_count, created_at) VALUES (1, 1, 1, 0);
             INSERT INTO source_links (id, batch_id, ordinal, input_url)
                 VALUES (11, 1, 0, 'https://files.example/Existing.zip?secret=private');
             INSERT INTO jobs (id, source_link_id, package_id, desired_state, stage,
                 generation, revision, created_at, updated_at)
                 VALUES (21, 11, 1, 'running', 'queued', 3, 9, 0, 0);",
        ).unwrap();
        conn.execute_batch(
            "INSERT INTO staging_files (job_id, generation, root_id, name, file_device,
                 file_inode, boot_epoch, created_at, representation_known, suggested_name)
                 VALUES (21, 1, 1, 'stage-1', 1, 1, 1, 0, 1, 'Older.zip'),
                        (21, 2, 1, 'stage-2', 1, 2, 1, 0, 1, 'Existing Response.zip');",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        let saved: (i64, i64, i64, Option<String>, String) = conn
            .query_row(
                "SELECT j.id, j.generation, j.revision, j.suggested_name, s.input_url \
             FROM jobs j JOIN source_links s ON s.id = j.source_link_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            saved,
            (
                21,
                3,
                9,
                Some("Existing Response.zip".into()),
                "https://files.example/Existing.zip?secret=private".into()
            )
        );
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
    }

    #[test]
    fn download_removal_upgrade_hides_only_started_sources_without_changing_identity() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATIONS[..13])
            .to_latest(&mut conn)
            .unwrap();
        conn.execute_batch(
            "INSERT INTO packages (id, name, created_at) VALUES (1, 'p', 0);
             INSERT INTO input_batches (id, package_id, input_count, created_at) VALUES (1, 1, 2, 0);
             INSERT INTO source_links (id, batch_id, ordinal, input_url, held, listed, revision)
                 VALUES (11, 1, 0, 'https://files.example/started', 0, 1, 5),
                        (12, 1, 1, 'https://files.example/held', 1, 1, 7);
             INSERT INTO jobs (id, source_link_id, package_id, desired_state, stage,
                 generation, revision, created_at, updated_at)
                 VALUES (21, 11, 1, 'running', 'queued', 3, 9, 0, 0);",
        ).unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        let sources = conn
            .prepare("SELECT id, held, listed, revision FROM source_links ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(sources, [(11, 0, 0, 5), (12, 1, 1, 7)]);
        let job: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT id,source_link_id,generation,revision FROM jobs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(job, (21, 11, 3, 9));
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
    }

    #[test]
    fn existing_jobs_download_directly_after_the_provider_upgrade() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATIONS[..11])
            .to_latest(&mut conn)
            .unwrap();
        conn.execute_batch(
            "INSERT INTO packages (id, name, created_at) VALUES (1, 'p', 0); \
             INSERT INTO input_batches (id, package_id, input_count, created_at) \
                 VALUES (1, 1, 1, 0); \
             INSERT INTO source_links (id, batch_id, ordinal, input_url) \
                 VALUES (1, 1, 0, 'https://files.example/a'); \
             INSERT INTO jobs (id, source_link_id, package_id, desired_state, stage, \
                 generation, revision, created_at, updated_at) \
                 VALUES (1, 1, 1, 'running', 'queued', 1, 1, 0, 0);",
        )
        .unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        let route: (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT route_service, route_account FROM jobs WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(route, (None, None));
        // A route names both its service and its account, or neither.
        assert!(
            conn.execute(
                "UPDATE jobs SET route_service = 'real-debrid' WHERE id = 1",
                []
            )
            .is_err()
        );
        conn.execute(
            "UPDATE jobs SET route_service = 'real-debrid', route_account = 3 WHERE id = 1",
            [],
        )
        .unwrap();
        // One open operation per job, and a link only in a succeeded one.
        let intend = "INSERT INTO provider_operations \
             (job_id, generation, service, account_id, state, started_at) \
             VALUES (1, 1, 'real-debrid', 3, 'intended', 0)";
        conn.execute(intend, []).unwrap();
        assert!(conn.execute(intend, []).is_err());
        assert!(
            conn.execute(
                "UPDATE provider_operations SET reference = zeroblob(16), sealed = zeroblob(40)",
                []
            )
            .is_err()
        );
    }

    #[test]
    fn the_speed_limit_upgrade_preserves_settings_and_provider_state() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATIONS[..12])
            .to_latest(&mut conn)
            .unwrap();
        conn.execute(
            "UPDATE settings SET revision = 5, max_active_downloads = 2 WHERE id = 1",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, service, revision, credential_generation, enabled, \
                 status, added_at, sign_in) VALUES (3, 'real-debrid', 7, 4, 1, 'authenticated', 0, 2); \
             INSERT INTO account_credentials (account_id, reference, format, sealed) \
                 VALUES (3, zeroblob(16), 1, zeroblob(40)); \
             INSERT INTO packages (id, name, created_at) VALUES (1, 'p', 0); \
             INSERT INTO input_batches (id, package_id, input_count, created_at) VALUES (1, 1, 1, 0); \
             INSERT INTO source_links (id, batch_id, ordinal, input_url) \
                 VALUES (1, 1, 0, 'https://files.example/a'); \
             INSERT INTO jobs (id, source_link_id, package_id, desired_state, stage, \
                 generation, revision, created_at, updated_at, route_service, route_account) \
                 VALUES (1, 1, 1, 'running', 'queued', 2, 3, 0, 0, 'real-debrid', 3); \
             INSERT INTO provider_operations (job_id, generation, service, account_id, state, \
                 started_at, reference, sealed, sign_in) \
                 VALUES (1, 2, 'real-debrid', 3, 'succeeded', 0, zeroblob(16), zeroblob(40), 2);",
        )
        .unwrap();
        // Compare all provider state, including opaque encrypted records,
        // before and after applying the new settings migration twice.
        let provider_state = |conn: &Connection| {
            [
                "accounts",
                "account_credentials",
                "jobs",
                "provider_operations",
            ]
            .map(|table| {
                let mut query = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                let columns = query.column_count();
                query
                    .query_map([], |row| {
                        (0..columns)
                            .map(|i| row.get::<_, rusqlite::types::Value>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            })
        };
        let mut before = provider_state(&conn);
        // The later naming migration adds an empty hint to this queued job,
        // and the media migration an empty media service and choice.
        before[2][0].extend(std::iter::repeat_n(rusqlite::types::Value::Null, 3));
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
        assert_eq!(provider_state(&conn), before);
        let saved: (i64, i64, Option<i64>) = conn
            .query_row(
                "SELECT revision, max_active_downloads, download_speed_limit FROM settings",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(saved, (5, 2, None));
        assert!(
            conn.execute("UPDATE settings SET download_speed_limit = -1", [])
                .is_err()
        );
    }
}
