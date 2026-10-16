-- Wayfetch state schema, migration 12: downloads through a provider
-- account, and the durable record of each provider operation
-- (architecture brief sections 6.1, 6.2, 6.5, ADR-09, ADR-24; test T12).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- The route a job was started through: the provider service and account
-- the user chose, or neither for a direct download. A soft reference: the
-- account may be removed later, and the job then says so instead of
-- blocking the removal.
ALTER TABLE jobs ADD COLUMN route_service TEXT
    CHECK (route_service IS NULL OR length(route_service) BETWEEN 1 AND 64);
ALTER TABLE jobs ADD COLUMN route_account INTEGER
    CHECK ((route_account IS NULL) = (route_service IS NULL) AND
           (route_account IS NULL OR route_account > 0));

-- One provider operation per row: the intent is recorded before the
-- request is sent, and the outcome before the transfer starts.
--
-- * `intended`: sent, or about to be; the outcome is not known.
-- * `succeeded`: the provider generated a link, sealed here with the
--   vault's key (`reference`, `sealed`) until it is used up or refused.
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
    CHECK ((state = 'succeeded') = (sealed IS NOT NULL)),
    CHECK ((reference IS NULL) = (sealed IS NULL))
) STRICT;

CREATE UNIQUE INDEX provider_operations_open ON provider_operations (job_id)
    WHERE state IN ('intended', 'succeeded', 'ambiguous', 'confirmed');
CREATE INDEX provider_operations_account ON provider_operations (account_id)
    WHERE state IN ('intended', 'succeeded', 'ambiguous', 'confirmed');
