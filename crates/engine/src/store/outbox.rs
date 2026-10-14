//! Durable event outbox (architecture brief section 7.8, ADR-28).
//!
//! Events are appended inside the transaction that makes the change they
//! describe, so an event exists exactly when its change committed. Positions
//! come from SQLite `AUTOINCREMENT` under the single writer: they increase by
//! one per event and are never reused.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::event::EventKind;

use super::StoreError;

/// Appends an event in the caller's transaction and returns its position.
pub(super) fn append(
    tx: &Transaction<'_>,
    kind: &EventKind,
    now: UtcTimestamp,
) -> Result<u64, StoreError> {
    let json = serde_json::to_string(kind).map_err(|_| StoreError::Corrupt("event"))?;
    tx.execute(
        "INSERT INTO event_outbox (kind, created_at) VALUES (?1, ?2)",
        params![json, now.unix_millis()],
    )?;
    u64::try_from(tx.last_insert_rowid()).map_err(|_| StoreError::Corrupt("event position"))
}

/// Position of the newest event ever appended, or 0. Counts pruned events,
/// so it never moves backwards.
pub(super) fn head(tx: &Transaction<'_>) -> Result<u64, StoreError> {
    let seq: Option<i64> = tx
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name = 'event_outbox'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    u64::try_from(seq.unwrap_or(0)).map_err(|_| StoreError::Corrupt("event position"))
}

#[cfg(test)]
pub(super) fn after(tx: &Transaction<'_>, position: u64) -> Vec<(u64, EventKind)> {
    let mut stmt = tx
        .prepare("SELECT position, kind FROM event_outbox WHERE position > ?1 ORDER BY position")
        .unwrap();
    stmt.query_map([i64::try_from(position).unwrap()], |row| {
        let position: i64 = row.get(0)?;
        let kind: String = row.get(1)?;
        Ok((
            u64::try_from(position).unwrap(),
            serde_json::from_str(&kind).unwrap(),
        ))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}
