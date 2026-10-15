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
    M::up(include_str!("migrations/0010_accounts.sql")),
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
}
