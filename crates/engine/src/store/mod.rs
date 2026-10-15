//! The engine's durable state (architecture brief sections 6.2 and 7.6 to
//! 7.8, ADR-02, ADR-04 and ADR-16).
//!
//! One [`Store`] owns the SQLite database inside the locked state directory.
//! Only the coordinator holds it, so every write is serialized through one
//! connection.
//!
//! # Durability profile
//!
//! The database uses WAL with `synchronous=FULL` and foreign keys, and
//! refuses a linked SQLite without the WAL-reset fix (ADR-04). A write is
//! reported only after its transaction commits.
//!
//! # Transaction boundaries
//!
//! Each write operation is one `IMMEDIATE` transaction that commits the
//! state change together with its outbox events and, for client commands,
//! the idempotency record:
//!
//! * [`Store::execute`]: a client command, its outbox events, and its
//!   idempotency record.
//! * [`Store::stage_pending`]: jobs for submitted inputs, with `job_added`
//!   events.
//! * [`Store::advance_job`]: one generation-checked stage transition and its
//!   event.
//! * [`Store::recover_after_restart`]: every active job moved to recovery,
//!   with their events.
//! * [`Store::commit_checkpoint`]: synchronized byte ranges for one partial.
//! * [`Store::record_response`]: what the first response reported about a
//!   staging file's bytes, and the job's expected size.
//! * [`Store::prune`]: events and idempotency records past retention.
//! * [`Store::register_staging`] and [`Store::adopt_staging`]: which job
//!   generation owns a staging file. Registering a newer generation's file,
//!   and queueing a job under a new generation, mark the job's older files
//!   abandoned in the same transaction.
//! * [`Store::finish_staging_cleanup`]: what removing an abandoned staging
//!   file found.
//! * `Store::allocate_root`, `Store::adopt_root`, `Store::register_root`,
//!   and `Store::forget_root`: one root recorded by path.
//! * [`Store::begin_publication`], [`Store::complete_publication`] (with
//!   the job's completion), and [`Store::abandon_publication`]: the
//!   publication intent around the filesystem link.
//!
//! Reads run in a short deferred transaction so a page and its `as_of`
//! cursor agree, and never span a network request or a subscription.
//!
//! # Schema migrations
//!
//! See [`schema`]: numbered, forward-only, append-only once merged, and an
//! engine refuses a database written by a newer schema.

mod commands;
mod files;
mod jobs;
mod outbox;
mod progress;
mod renames;
mod roots;
#[cfg(test)]
mod scenarios;
pub mod schema;

pub use files::Partial;
pub use jobs::Admission;
pub use renames::{RenameIntent, RenameStart};
pub(crate) use roots::path_text;

use std::path::Path;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use wayfetch_domain::id::{BatchId, JobId, PackageId, RootId, SourceLinkId};
use wayfetch_domain::job::JobError;
use wayfetch_domain::version::{BootEpoch, Generation, Revision};
use wayfetch_protocol::{ErrorCode, ProtocolError};

/// File name of the database inside the state directory.
pub const DATABASE_FILE: &str = "state.sqlite3";

/// Why a storage operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite reported an error.
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A schema migration failed.
    #[error("schema migration: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    /// The database was written by a newer engine.
    #[error(
        "database schema version {found} is newer than the {supported} this engine supports; \
         refusing to downgrade"
    )]
    SchemaTooNew {
        /// The database's schema version.
        found: usize,
        /// The newest version this engine knows.
        supported: usize,
    },
    /// The linked SQLite lacks a required fix.
    #[error("linked SQLite {0} lacks the WAL-reset fix; use 3.51.3 or later")]
    UnsupportedSqlite(&'static str),
    /// The database refused the WAL journal mode, for example on a network
    /// filesystem.
    #[error("database journal mode is {0}, not wal")]
    JournalMode(String),
    /// A stored value is invalid.
    #[error("stored {0} is invalid")]
    Corrupt(&'static str),
    /// The record does not exist.
    #[error("{0} not found")]
    NotFound(&'static str),
    /// The domain rejected the transition.
    #[error(transparent)]
    Job(#[from] JobError),
    /// A checkpoint was rejected.
    #[error("checkpoint rejected: {0}")]
    Checkpoint(&'static str),
    /// A staging or publication record was refused.
    #[error("staging rejected: {0}")]
    Staging(&'static str),
    /// Another publication already reserved the target, or a name that
    /// collides with it on a case-insensitive filesystem.
    #[error("target {0} is already reserved")]
    TargetReserved(crate::fs::RelativePath),
    /// A file name was refused by the filename policy.
    #[error("invalid file name: {0}")]
    InvalidName(crate::fs::PathError),
    /// The record is in the middle of a change. Retry later.
    #[error("{0}")]
    Busy(&'static str),
    /// A parked stage was requested without the failure that explains it.
    #[error("moving to {0} requires a failure")]
    MissingFailure(wayfetch_domain::job::JobStage),
    /// A root cannot be recorded.
    #[error("root rejected: {0}")]
    Root(&'static str),
    /// A sentinel names a root recorded at another path.
    #[error("the folder is marked as {id}, which is recorded at {recorded}")]
    RootMoved {
        /// The root the sentinel names.
        id: RootId,
        /// Where the store records it.
        recorded: String,
    },
}

impl From<StoreError> for ProtocolError {
    /// Client-facing form. Storage faults are reported as `internal` without
    /// details, which belong in engine diagnostics.
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Job(e) => e.into(),
            StoreError::NotFound(what) => {
                ProtocolError::new(ErrorCode::NotFound, &format!("{what} not found"))
            }
            StoreError::Root(_) | StoreError::InvalidName(_) => {
                ProtocolError::new(ErrorCode::InvalidRequest, &err.to_string())
            }
            StoreError::TargetReserved(_) => ProtocolError::new(
                ErrorCode::AlreadyExists,
                "another download already has that name",
            ),
            StoreError::Busy(why) => ProtocolError::new(ErrorCode::Busy, why),
            StoreError::RootMoved { .. } => {
                ProtocolError::new(ErrorCode::NotApplicable, &err.to_string())
            }
            _ => ProtocolError::new(ErrorCode::Internal, "storage failure"),
        }
    }
}

/// Settings the database runs with, reported at startup (ADR-04 gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreInfo {
    /// Version of the linked SQLite library.
    pub sqlite_version: &'static str,
    /// Journal mode in effect.
    pub journal_mode: String,
    /// `synchronous` level in effect (2 is FULL).
    pub synchronous: i64,
    /// Schema version after migrations.
    pub schema_version: usize,
}

/// The engine's database. Owned by the coordinator; not shared.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    info: StoreInfo,
}

impl Store {
    /// Opens or creates the database in `state_dir`, applies the durability
    /// profile, and migrates the schema. Blocking.
    pub fn open(state_dir: &Path) -> Result<Self, StoreError> {
        check_sqlite_version(rusqlite::version_number())?;
        let mut conn = Connection::open_with_flags(
            state_dir.join(DATABASE_FILE),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let journal_mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "wal", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::JournalMode(journal_mode));
        }
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "trusted_schema", false)?;
        let schema_version = schema::migrate(&mut conn)?;
        let synchronous = conn.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        let info = StoreInfo {
            sqlite_version: rusqlite::version(),
            journal_mode,
            synchronous,
            schema_version,
        };
        Ok(Self { conn, info })
    }

    /// Settings in effect.
    pub fn info(&self) -> &StoreInfo {
        &self.info
    }

    /// Position of the newest durable event, or 0.
    pub fn event_head(&mut self) -> Result<u64, StoreError> {
        let tx = self.read()?;
        outbox::head(&tx)
    }

    fn write(&mut self) -> Result<Transaction<'_>, StoreError> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?)
    }

    fn read(&mut self) -> Result<Transaction<'_>, StoreError> {
        Ok(self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?)
    }
}

/// Whether `version` (SQLite's `X*1000000 + Y*1000 + Z`) has the WAL-reset
/// fix: 3.51.3 or later, or the fixed 3.44.6 and 3.50.7 backports.
fn has_wal_reset_fix(version: i32) -> bool {
    let (minor, patch) = ((version / 1000) % 1000, version % 1000);
    version >= 3_051_003
        || (version / 1_000_000 == 3 && minor == 44 && patch >= 6)
        || (version / 1_000_000 == 3 && minor == 50 && patch >= 7)
}

fn check_sqlite_version(version: i32) -> Result<(), StoreError> {
    if has_wal_reset_fix(version) {
        Ok(())
    } else {
        Err(StoreError::UnsupportedSqlite(rusqlite::version()))
    }
}

/// A positive 64-bit domain value stored as an SQLite integer.
trait Key: Copy {
    const WHAT: &'static str;
    fn to_u64(self) -> u64;
    fn from_u64(value: u64) -> Option<Self>;
}

macro_rules! impl_key {
    ($($ty:ty => $what:literal),* $(,)?) => {$(
        impl Key for $ty {
            const WHAT: &'static str = $what;
            fn to_u64(self) -> u64 {
                self.get()
            }
            fn from_u64(value: u64) -> Option<Self> {
                <$ty>::from_u64(value)
            }
        }
    )*};
}

impl_key!(
    BatchId => "batch id",
    JobId => "job id",
    PackageId => "package id",
    RootId => "root id",
    SourceLinkId => "source link id",
    Generation => "generation",
    BootEpoch => "boot epoch",
    Revision => "revision",
);

/// SQLite form of a key. Values beyond `i64::MAX` cannot be stored.
fn sql<K: Key>(key: K) -> Result<i64, StoreError> {
    i64::try_from(key.to_u64()).map_err(|_| StoreError::Corrupt(K::WHAT))
}

/// Reads a key back, rejecting zero and negative values.
fn key<K: Key>(value: i64) -> Result<K, StoreError> {
    u64::try_from(value)
        .ok()
        .and_then(K::from_u64)
        .ok_or(StoreError::Corrupt(K::WHAT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_reset_fix_gate() {
        for ok in [3_051_003, 3_053_002, 3_044_006, 3_050_007, 4_000_000] {
            assert!(has_wal_reset_fix(ok), "{ok}");
        }
        for bad in [3_051_002, 3_050_006, 3_044_005, 3_045_000, 3_049_000] {
            assert!(!has_wal_reset_fix(bad), "{bad}");
        }
        assert!(check_sqlite_version(rusqlite::version_number()).is_ok());
    }

    #[test]
    fn opens_with_the_durability_profile() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let info = store.info();
        assert_eq!(info.journal_mode, "wal");
        assert_eq!(info.synchronous, 2);
        assert_eq!(info.schema_version, schema::LATEST);
        let fk: bool = store
            .conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(fk);
    }

    #[test]
    fn keys_reject_unrepresentable_values() {
        assert!(key::<JobId>(0).is_err());
        assert!(key::<JobId>(-1).is_err());
        assert_eq!(key::<JobId>(7).unwrap().get(), 7);
        let huge = JobId::from_u64(u64::MAX).unwrap();
        assert!(sql(huge).is_err());
    }
}
