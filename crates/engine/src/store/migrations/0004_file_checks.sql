-- Checking complete downloads against checksums (on unless turned off), and
-- downloading a file that fails its check again once on its own (off unless
-- turned on).
ALTER TABLE settings ADD COLUMN check_files INTEGER NOT NULL DEFAULT 1
    CHECK (check_files IN (0, 1));
ALTER TABLE settings ADD COLUMN redownload_failed_checks INTEGER NOT NULL DEFAULT 0
    CHECK (redownload_failed_checks IN (0, 1));

-- The published file's modification time in nanoseconds since the Unix
-- epoch, as seen right after it was published; NULL if it was not seen. A
-- later check or replacement treats a different time as a change made
-- outside Wayfetch.
ALTER TABLE publications ADD COLUMN file_modified_ns INTEGER;

-- How many times the job was downloaded again on its own after its file
-- failed a check, over its whole life, so several checksum files cannot
-- each start another download.
ALTER TABLE jobs ADD COLUMN check_redownloads INTEGER NOT NULL DEFAULT 0
    CHECK (check_redownloads >= 0);

-- Checksums the provider gave for the bytes of this staging file, as
-- `algorithm:hex` separated by spaces, checked before it is published.
ALTER TABLE staging_files ADD COLUMN checksums TEXT CHECK (length(checksums) <= 512);

-- A complete download whose file is a checksum file, and whether it was
-- read. `publication_id` is the publication read: a soft reference, so a
-- replaced or deleted file leaves rows that no longer apply instead of
-- blocking anything. `skipped` counts lines that were not entries.
CREATE TABLE checksum_files (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id),
    publication_id INTEGER NOT NULL,
    package_id INTEGER NOT NULL,
    root_id INTEGER NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 255),
    state TEXT NOT NULL CHECK (state IN ('pending', 'read', 'unreadable')),
    skipped INTEGER NOT NULL DEFAULT 0 CHECK (skipped >= 0),
    problem TEXT CHECK (length(problem) <= 512),
    updated_at INTEGER NOT NULL
) STRICT;

CREATE INDEX checksum_files_by_package ON checksum_files (package_id, state);

-- What a checksum file read lists: the path below its root an entry names
-- (the checksum file's folder joined with the entry's name), its
-- case-folded form, and the value. Names are only matched against
-- recorded downloads; nothing is opened by one.
CREATE TABLE checksum_entries (
    publication_id INTEGER NOT NULL,
    seq INTEGER NOT NULL CHECK (seq >= 0),
    path TEXT NOT NULL CHECK (length(path) <= 4096),
    path_key TEXT NOT NULL,
    algorithm TEXT NOT NULL CHECK (algorithm IN ('crc32', 'md5', 'sha1', 'sha256')),
    value TEXT NOT NULL CHECK (length(value) <= 64),
    PRIMARY KEY (publication_id, seq)
) STRICT, WITHOUT ROWID;

CREATE INDEX checksum_entries_by_key ON checksum_entries (path_key);

-- Checks of a published file (`publication_id`, a soft reference) against
-- one source: a checksum file's publication, or 0 for the provider. Bound to
-- the job generation that published the file and, once checked, to the
-- file's identity, size, and modification time as the check saw them, so a
-- result never carries over to a replacement or another version.
CREATE TABLE verifications (
    publication_id INTEGER NOT NULL,
    source_publication_id INTEGER NOT NULL CHECK (source_publication_id >= 0),
    algorithm TEXT NOT NULL CHECK (algorithm IN ('crc32', 'md5', 'sha1', 'sha256')),
    job_id INTEGER NOT NULL REFERENCES jobs (id),
    generation INTEGER NOT NULL CHECK (generation > 0),
    source_name TEXT CHECK (length(source_name) <= 255),
    expected TEXT NOT NULL CHECK (length(expected) <= 64),
    state TEXT NOT NULL CHECK (state IN ('pending', 'verified', 'failed', 'conflict', 'changed')),
    actual TEXT CHECK (length(actual) <= 64),
    file_device INTEGER,
    file_inode INTEGER,
    file_birth_ns INTEGER,
    file_size INTEGER,
    file_modified_ns INTEGER,
    problem TEXT CHECK (length(problem) <= 512),
    checked_at INTEGER,
    PRIMARY KEY (publication_id, source_publication_id, algorithm)
) STRICT, WITHOUT ROWID;

CREATE INDEX verifications_by_state ON verifications (state, job_id);
CREATE INDEX verifications_by_job ON verifications (job_id);

-- A complete download whose file failed its check, authorized to be
-- downloaded again. The file is removed only while it is still the
-- publication's file with the modification time the failing check saw;
-- then, in one transaction, the publication and its checks are dropped and
-- the job is queued under a new generation. A crash in between is finished
-- at the next start. `idempotency_key` and `fingerprint` are a person's
-- retry, whose result is recorded when it finishes; NULL for an automatic
-- one.
CREATE TABLE replacements (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id),
    publication_id INTEGER NOT NULL UNIQUE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    file_modified_ns INTEGER,
    reason TEXT NOT NULL CHECK (length(reason) <= 512),
    idempotency_key TEXT UNIQUE,
    fingerprint BLOB CHECK (fingerprint IS NULL OR length(fingerprint) = 32),
    created_at INTEGER NOT NULL,
    CHECK ((idempotency_key IS NULL) = (fingerprint IS NULL))
) STRICT;
