//! Durable event outbox (architecture brief section 7.8, ADR-28).
//!
//! Events are appended inside the transaction that makes the change they
//! describe, so an event exists exactly when its change committed. Positions
//! come from SQLite `AUTOINCREMENT` under the single writer: they increase by
//! one per event and are never reused.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::event::EventKind;

use super::{Store, StoreError};

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

/// Up to `limit` events after `position`, oldest first.
fn after(
    tx: &Transaction<'_>,
    position: u64,
    limit: usize,
) -> Result<Vec<(u64, EventKind)>, StoreError> {
    let position = i64::try_from(position).unwrap_or(i64::MAX);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    tx.prepare(
        "SELECT position, kind FROM event_outbox WHERE position > ?1 ORDER BY position LIMIT ?2",
    )?
    .query_map(params![position, limit], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?
    .map(|row| {
        let (position, kind) = row?;
        Ok((
            u64::try_from(position).map_err(|_| StoreError::Corrupt("event position"))?,
            serde_json::from_str(&kind).map_err(|_| StoreError::Corrupt("event"))?,
        ))
    })
    .collect()
}

impl Store {
    /// Up to `limit` durable events after `position`, oldest first.
    pub fn events_after(
        &mut self,
        position: u64,
        limit: usize,
    ) -> Result<Vec<(u64, EventKind)>, StoreError> {
        let tx = self.read()?;
        after(&tx, position, limit)
    }

    /// Which events can still be replayed: everything after the first
    /// returned position, up to the second (the head).
    pub fn retained_events(&mut self) -> Result<(u64, u64), StoreError> {
        let tx = self.read()?;
        let head = head(&tx)?;
        let oldest: Option<i64> =
            tx.query_row("SELECT MIN(position) FROM event_outbox", [], |row| {
                row.get(0)
            })?;
        let pruned_through = match oldest {
            Some(oldest) => {
                u64::try_from(oldest - 1).map_err(|_| StoreError::Corrupt("event position"))?
            }
            // Nothing is retained, so nothing up to the head can be replayed.
            None => head,
        };
        Ok((pruned_through, head))
    }

    /// Keeps the newest `keep_events` events and the idempotency records
    /// created at or after `commands_since`, in one transaction. Positions
    /// and the head are unaffected; a subscriber asking for pruned events
    /// is told to resynchronize. Returns the events and records removed.
    pub fn prune(
        &mut self,
        keep_events: u64,
        commands_since: UtcTimestamp,
    ) -> Result<(usize, usize), StoreError> {
        let tx = self.write()?;
        let cutoff = head(&tx)?.saturating_sub(keep_events);
        let events = tx.execute(
            "DELETE FROM event_outbox WHERE position <= ?1",
            [i64::try_from(cutoff).map_err(|_| StoreError::Corrupt("event position"))?],
        )?;
        let commands = tx.execute(
            "DELETE FROM commands WHERE created_at < ?1",
            [commands_since.unix_millis()],
        )?;
        tx.commit()?;
        Ok((events, commands))
    }
}

#[cfg(test)]
pub(super) fn all(tx: &Transaction<'_>) -> Vec<(u64, EventKind)> {
    after(tx, 0, usize::MAX).unwrap()
}
