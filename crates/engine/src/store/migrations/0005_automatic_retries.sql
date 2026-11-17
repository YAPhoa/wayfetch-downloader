-- Automatic retries of a transient failure (architecture section 7.4). A
-- job waiting for one is queued with its failure's retry_at as deadline.
-- The retries made in a row without new durable data; reset when one adds
-- some, when the job gives up, and when the user retries it.
ALTER TABLE jobs ADD COLUMN retry_attempt INTEGER NOT NULL DEFAULT 0
    CHECK (retry_attempt >= 0);
-- While waiting: the most retries in a row allowed when it was scheduled.
ALTER TABLE jobs ADD COLUMN retry_limit INTEGER CHECK (retry_limit >= 0);
-- While waiting: the end of the wait the server asked for (Retry-After),
-- which a manual retry respects too; NULL if it asked for none.
ALTER TABLE jobs ADD COLUMN retry_not_before INTEGER;
-- The coordinator reads the next deadline instead of keeping a timer per
-- waiting job.
CREATE INDEX jobs_retry_deadline ON jobs (failure_retry_at)
    WHERE stage = 'queued' AND failure_retry_at IS NOT NULL;
