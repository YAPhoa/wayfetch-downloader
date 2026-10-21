//! Schema migrations: numbered SQL files under `store/migrations/`, applied
//! in order by [`rusqlite_migration`].
//!
//! Released migrations are never edited; a change is a new file. An engine
//! refuses a database newer than it knows
//! ([`super::StoreError::SchemaTooNew`]).

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations, SchemaVersion};

use super::StoreError;

const MIGRATIONS: &[M<'static>] = &[
    M::up(include_str!("migrations/0001_initial.sql")),
    M::up(include_str!("migrations/0002_root_initialized.sql")),
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
    fn a_route_and_its_provider_operation_are_constrained() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(migrate(&mut conn).unwrap(), LATEST);
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
        assert!(
            conn.execute("UPDATE settings SET download_speed_limit = -1", [])
                .is_err()
        );
    }
}
