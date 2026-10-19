-- Wayfetch state schema, migration 19: the waiting, needs_input, and
-- blocked stages are gone (protocol 5.0). A job that cannot continue until
-- the person acts is failed, its failure saying why; a temporary cooldown
-- is queued with the failure's retry time.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

UPDATE jobs SET stage = 'failed', revision = revision + 1,
    failure_code = COALESCE(failure_code, 'destination_unavailable')
WHERE stage IN ('waiting', 'needs_input', 'blocked');
