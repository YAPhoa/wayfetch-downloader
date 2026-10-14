-- Wayfetch state schema, migration 3: the representation a staging file
-- holds, the name its server suggested, and retention of idempotency
-- records (sections 7.5, 7.8, 10.3).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- What the first response said about the bytes in the staging file. A
-- resumed transfer continues only if the server still reports the same
-- representation (rules 7.5.4 and 7.5.5). NULL columns were not reported.
ALTER TABLE staging_files ADD COLUMN representation_length INTEGER
    CHECK (representation_length >= 0);
ALTER TABLE staging_files ADD COLUMN representation_etag TEXT
    CHECK (length(representation_etag) <= 1024);
ALTER TABLE staging_files ADD COLUMN representation_modified INTEGER;
-- The file name that response suggested, if any; a resumed file keeps it.
ALTER TABLE staging_files ADD COLUMN suggested_name TEXT
    CHECK (length(suggested_name) <= 1024);
-- 1 once a response was observed and the columns above were written.
ALTER TABLE staging_files ADD COLUMN representation_known INTEGER NOT NULL DEFAULT 0
    CHECK (representation_known IN (0, 1));

-- Idempotency records are pruned by age.
CREATE INDEX commands_by_age ON commands (created_at);

-- A job publishes at most one file.
CREATE UNIQUE INDEX publications_published ON publications (job_id)
    WHERE state = 'published';
