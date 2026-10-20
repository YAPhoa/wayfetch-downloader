-- Wayfetch state schema, migration 20: unpacking downloaded archives
-- (protocol 6.0).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- One row per archive download that unpacking was asked for, keyed by the
-- job that downloaded the archive (the first volume of a multi-part set).
-- Each attempt has a new generation, so a late result of an earlier one
-- cannot change a later one. `staging` names the attempt's private folder
-- under the root's .wayfetch/extract while it may exist; it is cleared
-- once removed or published. `output` is what a complete attempt made.
CREATE TABLE extractions (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    state TEXT NOT NULL CHECK (state IN (
        'waiting', 'queued', 'extracting', 'needs_password', 'complete', 'failed', 'cancelled'
    )),
    staging TEXT CHECK (length(staging) <= 64),
    output TEXT CHECK (length(output) <= 1024),
    problem TEXT CHECK (length(problem) <= 512),
    updated_at INTEGER NOT NULL,
    CHECK ((state = 'complete') = (output IS NOT NULL))
) STRICT;

CREATE INDEX extractions_by_state ON extractions (state, job_id);

-- Whether downloaded archives are unpacked as they finish; on unless the
-- user turns it off.
ALTER TABLE settings ADD COLUMN extract_archives INTEGER NOT NULL DEFAULT 1
    CHECK (extract_archives IN (0, 1));
