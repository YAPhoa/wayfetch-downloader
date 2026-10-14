-- Wayfetch state schema, migration 2: staging files and publication
-- intents (sections 7.6 and 7.7, ADR-16, ADR-17).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.
--
-- File identities are a device and inode, unsigned 64-bit values stored
-- with the same bits as signed integers, and a birth time in nanoseconds
-- where the filesystem reports one.

-- The private staging file a job generation writes, recorded only after the
-- file and its directory entry are durable. A file is adopted by a later
-- generation only after a restart: boot_epoch is the engine start that
-- created or last adopted it, so a file a live writer of this process may
-- still hold is never handed to another generation.
CREATE TABLE staging_files (
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    root_id INTEGER NOT NULL CHECK (root_id > 0),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    file_device INTEGER NOT NULL,
    file_inode INTEGER NOT NULL,
    file_birth_ns INTEGER,
    boot_epoch INTEGER NOT NULL CHECK (boot_epoch > 0),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (job_id, generation),
    UNIQUE (root_id, name)
) STRICT;

-- Publication intents and published artifacts. An intent is written before
-- the staging file is linked to its target and completed after, so a crash
-- in between is reconciled from the recorded identities. Targets are
-- reserved by their case-folded key per root, so two jobs never claim
-- names that collide on a case-insensitive filesystem.
CREATE TABLE publications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    root_id INTEGER NOT NULL CHECK (root_id > 0),
    staging_name TEXT NOT NULL,
    file_device INTEGER NOT NULL,
    file_inode INTEGER NOT NULL,
    file_birth_ns INTEGER,
    target_path TEXT NOT NULL,
    target_key TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    state TEXT NOT NULL CHECK (state IN ('intended', 'published')),
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    UNIQUE (root_id, target_key),
    CHECK ((state = 'published') = (completed_at IS NOT NULL))
) STRICT;

-- At most one unfinished publication per job.
CREATE UNIQUE INDEX publications_pending ON publications (job_id)
    WHERE state = 'intended';
