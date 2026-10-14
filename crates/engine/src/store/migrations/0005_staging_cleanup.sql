-- Wayfetch state schema, migration 5: staging files no generation can
-- reopen, and their cleanup (section 7.6).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- NULL while a generation of the job may still reopen or adopt the file.
-- 'pending' once none can: a newer generation registered its own file, or
-- the job went back to the queue to start from zero. The file is then
-- removed after its identity is checked, and the record after the file.
-- 'foreign' if the name was found holding another file, which cleanup
-- leaves in place; the record stays so the name is never mistaken for an
-- unrecorded file of the engine's own.
ALTER TABLE staging_files ADD COLUMN cleanup TEXT
    CHECK (cleanup IN ('pending', 'foreign'));

CREATE INDEX staging_files_pending ON staging_files (root_id)
    WHERE cleanup = 'pending';
