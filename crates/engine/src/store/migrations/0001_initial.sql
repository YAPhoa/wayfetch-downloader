-- Wayfetch state schema, version 0.1: the whole schema of the first
-- release, in one migration.
--
-- Migrations are append-only once released: never edit this file, add the
-- next numbered file instead. See the store::schema module.
--
-- Identifiers are store-allocated and never reused (AUTOINCREMENT). Revisions
-- and generations are positive. Byte offsets are half-open ranges within
-- 0..=i64::MAX, checked again by the application. File identities are a
-- device and inode, unsigned 64-bit values stored with the same bits as
-- signed integers, and a birth time in nanoseconds where the filesystem
-- reports one.

CREATE TABLE packages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    created_at INTEGER NOT NULL
) STRICT;

-- `root_id` is the destination chosen when the batch was submitted, copied
-- to each of its jobs, so a later change of the default never moves a job.
-- NULL for a batch submitted while the operator's download root was not
-- identified yet; its jobs belong to the download root and are given its ID
-- when first admitted.
--
-- `mode` is how far the batch's links are looked into: 'analyze' checks
-- every link and reads a typed link that is a web page; 'supported_only'
-- checks only links that look like files or videos.
CREATE TABLE input_batches (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL REFERENCES packages (id),
    input_count INTEGER NOT NULL CHECK (input_count > 0),
    created_at INTEGER NOT NULL,
    root_id INTEGER CHECK (root_id > 0),
    mode TEXT NOT NULL DEFAULT 'supported_only'
        CHECK (mode IN ('analyze', 'supported_only'))
) STRICT;

-- Links as the person submitted or collected them, and links found on
-- pages.
--
-- `listed` is 1 while a collected link is in the Links list, from
-- collecting until the user removes it or starts it; started links move to
-- Downloads and keep their row for the source identity of idempotent
-- starts. `held` is 1 while it waits to be started; the ingestion step
-- leaves it alone, and starting it creates its job and clears the flag in
-- one transaction. Links submitted to start at once are never listed.
-- `revision` changes when the user edits the link, so a command made from
-- an older view is refused. `root_id` is where it will be saved, copied to
-- its job; NULL for the operator's download root. It is looked up, not
-- enforced: a folder may stop being permitted while links name it, and its
-- `roots` row, which is never deleted, still gives its path. An entry that
-- is not a valid link is kept with `invalid` = 1 and stored without
-- credentials, and `problem` says why it is invalid or why starting it was
-- last refused.
CREATE TABLE source_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    batch_id INTEGER NOT NULL REFERENCES input_batches (id),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    input_url TEXT NOT NULL,
    -- 1 once the ingestion workflow has staged the link into a job.
    staged INTEGER NOT NULL DEFAULT 0 CHECK (staged IN (0, 1)),
    held INTEGER NOT NULL DEFAULT 0 CHECK (held IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    root_id INTEGER CHECK (root_id > 0),
    invalid INTEGER NOT NULL DEFAULT 0
        CHECK (invalid IN (0, 1) AND (invalid = 0 OR held = 1)),
    problem TEXT CHECK (length(problem) <= 512),
    listed INTEGER NOT NULL DEFAULT 0
        CHECK (listed IN (0, 1) AND (held = 0 OR listed = 1)),
    -- A link a media service recognized: the service, its check, the
    -- service's answer as JSON (title, duration, heights), and the
    -- person's choice as JSON; no choice means the default (best video).
    media_service TEXT
        CHECK (media_service IS NULL OR length(media_service) BETWEEN 1 AND 64),
    media_state TEXT CHECK (media_state IN ('pending', 'ready', 'failed')),
    media_info TEXT CHECK (media_info IS NULL OR length(media_info) <= 8192),
    media_problem TEXT,
    media_choice TEXT CHECK (media_choice IS NULL OR length(media_choice) <= 256),
    -- The page the link was found on, NULL for a link the person typed or
    -- pasted. A page with links found on it is never deleted while they
    -- are, only unlisted, so every link keeps where it came from.
    parent_id INTEGER REFERENCES source_links (id),
    -- 1 if the link may be read as a page when it is one: a link the person
    -- typed in a batch that analyzes, or one they asked to analyze. A link
    -- found on a page never may, so analysis never spiders.
    may_analyze INTEGER NOT NULL DEFAULT 0 CHECK (may_analyze IN (0, 1)),
    -- 1 if requests for the link, its checks and its download, may reach
    -- only public addresses: a link in pasted HTML, or one found on a page
    -- that was not itself served from a local address. The page's author
    -- chose it, not the person, so it never gets the local networks the
    -- operator allows for intentional downloads.
    public_only INTEGER NOT NULL DEFAULT 0 CHECK (public_only IN (0, 1)),
    -- What the last finished check found: NULL before any. A media link's
    -- check keeps the media columns instead.
    check_state TEXT CHECK (check_state IN ('online', 'offline', 'unknown', 'page')),
    check_reason TEXT
        CHECK (check_reason IS NULL OR length(check_reason) BETWEEN 1 AND 32),
    check_problem TEXT CHECK (check_problem IS NULL OR length(check_problem) <= 512),
    -- A file's size and suggested name, when its server said. NULL size is
    -- unknown, never zero.
    size INTEGER CHECK (size >= 0),
    observed_name TEXT
        CHECK (observed_name IS NULL OR length(observed_name) BETWEEN 1 AND 255),
    -- For a page that was read: links on it collected, and links to other
    -- pages left alone. NULL until it is read.
    found INTEGER CHECK (found >= 0),
    skipped INTEGER CHECK (skipped >= 0),
    -- The service whose provider answered the link's last check, for a link
    -- an enabled account would download; NULL when its own server did.
    checked_via TEXT CHECK (checked_via IS NULL OR length(checked_via) BETWEEN 1 AND 64),
    -- 1 if the link's own server last answered with a web page, kept apart
    -- from a provider's answer shown instead: starting it directly
    -- downloads the page, and is refused.
    origin_page INTEGER NOT NULL DEFAULT 0 CHECK (origin_page IN (0, 1)),
    UNIQUE (batch_id, ordinal)
) STRICT;

CREATE INDEX source_links_unstaged ON source_links (id) WHERE staged = 0;

CREATE INDEX source_links_listed ON source_links (id) WHERE listed = 1;

CREATE INDEX source_links_media_pending ON source_links (id)
    WHERE media_state = 'pending' AND listed = 1;

CREATE INDEX source_links_by_parent ON source_links (parent_id) WHERE parent_id IS NOT NULL;

-- Finds a link already collected in a batch, so one found on two pages is
-- listed once.
CREATE INDEX source_links_by_batch_url ON source_links (batch_id, input_url);

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
    -- The failure explaining a failed stage, or the cooldown of a queued
    -- one; all NULL otherwise. The failure's scope is not stored yet.
    failure_code TEXT,
    failure_retry_at INTEGER,
    failure_message TEXT CHECK (length(failure_message) <= 512),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- The destination copied from the batch; NULL until the operator's
    -- download root is identified. A job that wrote or published a file
    -- keeps the root that holds it.
    root_id INTEGER CHECK (root_id > 0),
    -- When the user removed the job from lists and counts. The job, its
    -- files, partial data, and publication records stay; NULL for a listed
    -- job.
    removed_at INTEGER,
    -- Why the job's latest attempt discarded its saved data and started
    -- from zero. Kept until the user retries the job again.
    restart_reason TEXT CHECK (length(restart_reason) <= 512),
    -- The route the job was started through: the provider service and
    -- account the user chose, or neither for a direct download. A soft
    -- reference: the account may be removed later, and the job then says
    -- so instead of blocking the removal.
    route_service TEXT
        CHECK (route_service IS NULL OR length(route_service) BETWEEN 1 AND 64),
    route_account INTEGER
        CHECK ((route_account IS NULL) = (route_service IS NULL) AND
               (route_account IS NULL OR route_account > 0)),
    -- A validated provider or response file name hint, available before
    -- publication. Hints from the source path are derived from the source
    -- link instead. Readers still apply the single-component name policy.
    suggested_name TEXT
        CHECK (suggested_name IS NULL OR length(suggested_name) BETWEEN 1 AND 255),
    -- A download of a media link keeps its service and choice.
    media_service TEXT
        CHECK (media_service IS NULL OR length(media_service) BETWEEN 1 AND 64),
    media_choice TEXT CHECK (media_choice IS NULL OR length(media_choice) <= 256),
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

-- Idempotency records of committed client commands, pruned by age.
CREATE TABLE commands (
    idempotency_key TEXT PRIMARY KEY,
    fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
    result TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE INDEX commands_by_age ON commands (created_at);

-- Durable events. AUTOINCREMENT keeps positions increasing even after
-- pruning removes every row, so a cursor is never reused.
CREATE TABLE event_outbox (
    position INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

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
    -- What the first response said about the bytes in the file. A resumed
    -- transfer continues only if the server still reports the same
    -- representation (rules 7.5.4 and 7.5.5). NULL columns were not
    -- reported.
    representation_length INTEGER CHECK (representation_length >= 0),
    representation_etag TEXT CHECK (length(representation_etag) <= 1024),
    representation_modified INTEGER,
    -- The file name that response suggested, if any; a resumed file keeps it.
    suggested_name TEXT CHECK (length(suggested_name) <= 1024),
    -- 1 once a response was observed and the representation was written.
    representation_known INTEGER NOT NULL DEFAULT 0
        CHECK (representation_known IN (0, 1)),
    -- An opaque identity of the resource whose response started the file,
    -- as the transport reports it (the HTTP transport records a digest of
    -- the final URL, never the URL, which may carry a signed token).
    -- Validators identify representations of one resource only, so a
    -- matching entity tag from another resource never makes its bytes
    -- interchangeable. NULL if the transport did not report it.
    representation_resource TEXT CHECK (length(representation_resource) <= 512),
    -- NULL while a generation of the job may still reopen or adopt the
    -- file. 'pending' once none can: a newer generation registered its own
    -- file, or the job went back to the queue to start from zero. The file
    -- is then removed after its identity is checked, and the record after
    -- the file. 'foreign' if the name was found holding another file, which
    -- cleanup leaves in place, as it does every file no record names.
    cleanup TEXT CHECK (cleanup IN ('pending', 'foreign')),
    -- The generation a manual retry reserved the file for, set in the
    -- retry's transaction on the job's newest retained file. Returning to
    -- the queue under that generation leaves the file to it instead of
    -- abandoning it; the attempt adopts it once the coordinator has proved
    -- no earlier task of the job still runs, and the adoption clears this.
    -- A newer generation that registers its own file, or a restart from
    -- zero under another generation, abandons it as usual.
    handoff_generation INTEGER CHECK (handoff_generation > generation),
    -- A media download's streams are joined or converted into a second
    -- file in the same staging folder. While converting, `aux_*` names the
    -- output being written; once converted, the output becomes the staging
    -- file and `aux_*` names the downloaded streams until they are removed.
    -- Every file stays named by a record, so cleanup can remove it by
    -- identity.
    media_phase TEXT CHECK (media_phase IN ('converting', 'converted')),
    aux_name TEXT CHECK (aux_name IS NULL OR length(aux_name) BETWEEN 1 AND 255),
    aux_device INTEGER,
    aux_inode INTEGER,
    aux_birth_ns INTEGER,
    PRIMARY KEY (job_id, generation),
    UNIQUE (root_id, name)
) STRICT;

CREATE INDEX staging_files_pending ON staging_files (root_id)
    WHERE cleanup = 'pending';

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

-- A job publishes at most one file.
CREATE UNIQUE INDEX publications_published ON publications (job_id)
    WHERE state = 'published';

-- Every root the engine has identified, by the path it was configured or
-- registered at. A root's identity is its sentinel, which names this ID;
-- the path only says where to look. `registered` is 1 for a root the host
-- added on the user's explicit request, such as a folder chosen in the
-- app, which stays a permitted destination; 0 for a root the operator
-- configured, which is permitted only while it is configured. `removed` is
-- 1 for a root the user removed: it is not permitted even while the
-- operator still configures its path, until the user adds the folder
-- again. A removed root is never `registered`.
CREATE TABLE roots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE CHECK (length(path) BETWEEN 1 AND 4096),
    registered INTEGER NOT NULL CHECK (registered IN (0, 1)),
    created_at INTEGER NOT NULL,
    removed INTEGER NOT NULL DEFAULT 0
        CHECK (removed IN (0, 1) AND NOT (removed = 1 AND registered = 1))
) STRICT;

-- ID 1 is the root that development builds before 0.1 marked with a fixed
-- ID. It is recorded from its sentinel when it is next configured, so new
-- roots start at 2 and never reuse it.
INSERT INTO sqlite_sequence (name, seq) VALUES ('roots', 1);

-- The user's settings: one row, edited with revision checks like a job.
-- `default_root` is where newly submitted jobs are saved; NULL means the
-- operator's configured download root.
--
-- The other columns are the user's saved preferences, one typed column
-- each. NULL means no saved preference: the operator's configured default,
-- else the built-in one, applies. A saved value is kept even when a later
-- policy no longer permits it; the engine then runs within the policy and
-- reports the conflict. The supported range is checked by the engine, so
-- it can change without a migration.
CREATE TABLE settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision > 0),
    default_root INTEGER REFERENCES roots (id),
    updated_at INTEGER NOT NULL,
    max_active_downloads INTEGER
        CHECK (max_active_downloads IS NULL OR max_active_downloads > 0),
    -- Bytes per second across every download. 0 is a saved Unlimited,
    -- distinct from NULL, and is read only by the store, which maps it to
    -- `SpeedLimit::Unlimited`; it is never compared as a rate.
    download_speed_limit INTEGER
        CHECK (download_speed_limit IS NULL OR download_speed_limit >= 0),
    -- Whether downloaded archives are unpacked as they finish; on unless
    -- the user turns it off.
    extract_archives INTEGER NOT NULL DEFAULT 1 CHECK (extract_archives IN (0, 1))
) STRICT;

INSERT INTO settings (id, revision, updated_at) VALUES (1, 1, 0);

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

-- A removal that deletes a job's files. The explicit authority to delete
-- survives a crash; the records of the files stay unchanged until every
-- deletion is durable, and only an explicit retry resumes one.
CREATE TABLE deletions (
    job_id INTEGER PRIMARY KEY REFERENCES jobs (id),
    revision INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32)
) STRICT;

-- The check of the key that encrypts credentials: an HMAC of a fixed label
-- under it, so a wrong key is recognized without decrypting anything. The
-- key itself is never stored here. One row, written when the first
-- account is added.
CREATE TABLE credential_vault (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    key_check BLOB NOT NULL CHECK (length(key_check) = 32),
    created_at INTEGER NOT NULL
) STRICT;

-- Accounts the user added, with only redacted metadata. `revision` guards
-- the user's edits; `credential_generation` advances whenever the stored
-- credentials change (signed in again, refreshed), so work started with
-- older credentials cannot store its result. `sign_in` advances only when
-- the account is signed in again, not by a refresh; a link is used only
-- while the sign-in that generated it is current. Status fields are what
-- the provider last said; unknown values stay NULL. AUTOINCREMENT keeps
-- IDs from being reused after a removal.
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    service TEXT NOT NULL CHECK (length(service) BETWEEN 1 AND 64),
    alias TEXT CHECK (alias IS NULL OR length(alias) BETWEEN 1 AND 64),
    revision INTEGER NOT NULL CHECK (revision > 0),
    credential_generation INTEGER NOT NULL CHECK (credential_generation > 0),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    status TEXT NOT NULL CHECK (
        status IN ('not_checked', 'authenticated', 'invalid_credentials', 'unavailable')
    ),
    problem TEXT CHECK (problem IS NULL OR length(problem) <= 512),
    checked_at INTEGER,
    provider_user TEXT CHECK (provider_user IS NULL OR length(provider_user) BETWEEN 1 AND 128),
    username TEXT CHECK (username IS NULL OR length(username) BETWEEN 1 AND 128),
    plan TEXT CHECK (plan IS NULL OR plan IN ('premium', 'free')),
    expires_at INTEGER,
    added_at INTEGER NOT NULL,
    sign_in INTEGER NOT NULL DEFAULT 1 CHECK (sign_in > 0),
    UNIQUE (service, provider_user)
) STRICT;

-- An account's credentials, sealed with the vault's key: a random nonce,
-- then ciphertext and tag. `reference` is the opaque secret reference the
-- seal's associated data binds; a record copied to another row does not
-- open. Bounded in size.
CREATE TABLE account_credentials (
    account_id INTEGER PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    reference BLOB NOT NULL UNIQUE CHECK (length(reference) = 16),
    format INTEGER NOT NULL CHECK (format = 1),
    sealed BLOB NOT NULL CHECK (length(sealed) BETWEEN 28 AND 8220)
) STRICT;

-- One provider operation per row: the intent is recorded before the
-- request is sent, and the outcome before the transfer starts.
--
-- * `intended`: sent, or about to be; the outcome is not known.
-- * `succeeded`: the provider generated a link, sealed here with the
--   vault's key (`reference`, `sealed`) until it is used up or refused;
--   `sign_in` is the account's sign-in it was generated or found with.
-- * `failed`: the provider certainly generated nothing.
-- * `ambiguous`: the answer was lost and no record shows the outcome; it
--   waits for the user to confirm a repeat.
-- * `confirmed`: the user confirmed; the next resolution reconciles once
--   more, then asks again.
-- * `retired`: no longer used; its link, if any, is erased.
--
-- At most one operation per job is open (any state but `failed` and
-- `retired`), so no local retry, restart, or replayed command can start a
-- second one beside it. `account_id` is a soft reference, like the job's.
CREATE TABLE provider_operations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    service TEXT NOT NULL CHECK (length(service) BETWEEN 1 AND 64),
    account_id INTEGER NOT NULL CHECK (account_id > 0),
    state TEXT NOT NULL CHECK (
        state IN ('intended', 'succeeded', 'failed', 'ambiguous', 'confirmed', 'retired')
    ),
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    reference BLOB CHECK (reference IS NULL OR length(reference) = 16),
    sealed BLOB CHECK (sealed IS NULL OR length(sealed) BETWEEN 28 AND 8220),
    problem TEXT CHECK (problem IS NULL OR length(problem) <= 512),
    sign_in INTEGER CHECK (sign_in IS NULL OR sign_in > 0),
    CHECK ((state = 'succeeded') = (sealed IS NOT NULL)),
    CHECK (state <> 'succeeded' OR sign_in IS NOT NULL),
    CHECK ((reference IS NULL) = (sealed IS NULL))
) STRICT;

CREATE UNIQUE INDEX provider_operations_open ON provider_operations (job_id)
    WHERE state IN ('intended', 'succeeded', 'ambiguous', 'confirmed');

CREATE INDEX provider_operations_account ON provider_operations (account_id)
    WHERE state IN ('intended', 'succeeded', 'ambiguous', 'confirmed');

-- The frontier: checks waiting or running, one per link. A row is the
-- check's identity: checking again replaces it, so the old check's late
-- answer finds no row and is dropped. A finished check deletes its row and
-- records what it found on the link in the same transaction. `queue` is
-- what checks are limited by, one at a time each: the link's host, or
-- 'media:' and its video service. Running rows are pending again after a
-- restart; a check only sends HEAD and GET, which may be repeated.
CREATE TABLE crawl_tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_link_id INTEGER NOT NULL UNIQUE REFERENCES source_links (id) ON DELETE CASCADE,
    queue TEXT NOT NULL CHECK (length(queue) BETWEEN 1 AND 320),
    state TEXT NOT NULL CHECK (state IN ('pending', 'running')),
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX crawl_tasks_pending ON crawl_tasks (queue, id) WHERE state = 'pending';

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
