-- Wayfetch state schema, migration 1.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.
--
-- Identifiers are store-allocated and never reused (AUTOINCREMENT). Revisions
-- and generations are positive. Byte offsets are half-open ranges within
-- 0..=i64::MAX, checked again by the application.

CREATE TABLE packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE input_batches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages (id),
    input_count INTEGER NOT NULL CHECK (input_count > 0),
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE source_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id INTEGER NOT NULL REFERENCES input_batches (id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    input_url TEXT NOT NULL,
    -- 1 once the ingestion workflow has staged the link into a job.
    staged INTEGER NOT NULL DEFAULT 0 CHECK (staged IN (0, 1)),
    UNIQUE (batch_id, ordinal)
) STRICT;

CREATE INDEX source_links_unstaged ON source_links (id) WHERE staged = 0;

-- At most one job per source link, so staging can be retried safely.
CREATE TABLE jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_link_id INTEGER NOT NULL UNIQUE REFERENCES source_links (id),
    package_id INTEGER NOT NULL REFERENCES packages (id),
    desired_state TEXT NOT NULL,
    stage TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    bytes_total INTEGER CHECK (bytes_total >= 0),
    -- The failure explaining a waiting, blocked, input-needed, or failed
    -- stage; all NULL otherwise. The failure's scope is not stored yet.
    failure_code TEXT,
    failure_retry_at INTEGER,
    failure_message TEXT CHECK (length(failure_message) <= 512),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (failure_code IS NOT NULL OR (failure_retry_at IS NULL AND failure_message IS NULL))
) STRICT;

CREATE INDEX jobs_by_package ON jobs (package_id, id);
CREATE INDEX jobs_by_stage ON jobs (stage, id);

-- Durable progress of the partial file owned by one job generation.
CREATE TABLE partials (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    checkpoint_seq INTEGER NOT NULL CHECK (checkpoint_seq > 0),
    durable_bytes INTEGER NOT NULL CHECK (durable_bytes >= 0),
    UNIQUE (job_id, generation)
) STRICT;

-- Disjoint, non-adjacent half-open ranges whose bytes were synchronized to
-- the partial file before the checkpoint committed.
CREATE TABLE durable_ranges (
    partial_id INTEGER NOT NULL REFERENCES partials (id) ON DELETE CASCADE,
    start_inclusive INTEGER NOT NULL CHECK (start_inclusive >= 0),
    end_exclusive INTEGER NOT NULL CHECK (end_exclusive > start_inclusive),
    PRIMARY KEY (partial_id, start_inclusive)
) STRICT, WITHOUT ROWID;

-- Idempotency records of committed client commands.
CREATE TABLE commands (
    idempotency_key TEXT PRIMARY KEY,
    fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
    result TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Durable events. AUTOINCREMENT keeps positions increasing even after
-- pruning removes every row, so a cursor is never reused.
CREATE TABLE event_outbox (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
