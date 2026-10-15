-- Wayfetch state schema, migration 6: destination roots, the default
-- destination, and the destination each job was submitted for (ADR-17,
-- ADR-29).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- Every root the engine has identified, by the path it was configured or
-- registered at. A root's identity is its sentinel, which names this ID;
-- the path only says where to look. `registered` is 1 for a root the host
-- added on the user's explicit request, such as a folder chosen in the
-- desktop app, which stays a permitted destination; 0 for a root the
-- operator configured, which is permitted only while it is configured.
CREATE TABLE roots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE CHECK (length(path) BETWEEN 1 AND 4096),
    registered INTEGER NOT NULL CHECK (registered IN (0, 1)),
    created_at INTEGER NOT NULL
) STRICT;

-- ID 1 is the root that earlier versions marked with a fixed ID. It is
-- recorded from its sentinel when it is next configured, so new roots
-- start at 2 and never reuse it.
INSERT INTO sqlite_sequence (name, seq) VALUES ('roots', 1);

-- The user's settings: one row, edited with revision checks like a job.
-- `default_root` is where newly submitted jobs are saved; NULL means the
-- operator's configured download root.
CREATE TABLE settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision > 0),
    default_root INTEGER REFERENCES roots (id),
    updated_at INTEGER NOT NULL
) STRICT;

INSERT INTO settings (id, revision, updated_at) VALUES (1, 1, 0);

-- The destination chosen when the batch was submitted, and copied to each
-- of its jobs, so a later change of the default never moves a job. NULL
-- for jobs of earlier versions and for jobs submitted while the operator's
-- download root was not identified yet; both belong to the download root
-- and are given its ID when first admitted.
ALTER TABLE input_batches ADD COLUMN root_id INTEGER CHECK (root_id > 0);
ALTER TABLE jobs ADD COLUMN root_id INTEGER CHECK (root_id > 0);

-- Jobs that already wrote or published a file keep the root that holds it.
UPDATE jobs SET root_id = COALESCE(
    (SELECT p.root_id FROM publications p WHERE p.job_id = jobs.id
     ORDER BY p.id DESC LIMIT 1),
    (SELECT s.root_id FROM staging_files s WHERE s.job_id = jobs.id
     ORDER BY s.generation DESC LIMIT 1));
