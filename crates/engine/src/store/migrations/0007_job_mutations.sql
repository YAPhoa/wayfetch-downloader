-- Wayfetch state schema, migration 7: renaming published files and
-- removing jobs from lists (ADR-16, ADR-23).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- When the user removed the job from lists and counts. The job, its files,
-- partial data, and publication records stay; NULL for a listed job.
ALTER TABLE jobs ADD COLUMN removed_at INTEGER;

-- A rename of a published file in progress, recorded before the new name
-- is linked and deleted when the publication records it, so a crash in
-- between is reconciled from the publication's file identity. The new
-- name's case-folded key is reserved like a publication target, and the
-- command's idempotency key and fingerprint are kept so that finishing the
-- rename after a restart still records the command's result.
CREATE TABLE renames (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id),
    publication_id INTEGER NOT NULL UNIQUE REFERENCES publications (id),
    root_id INTEGER NOT NULL CHECK (root_id > 0),
    target_path TEXT NOT NULL,
    target_key TEXT NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
    created_at INTEGER NOT NULL,
    UNIQUE (root_id, target_key)
) STRICT;
