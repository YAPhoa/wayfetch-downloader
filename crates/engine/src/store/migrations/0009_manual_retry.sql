-- Wayfetch state schema, migration 9: manual retry keeps a retained
-- partial for the next attempt, and a restart from zero says why.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- The generation a manual retry reserved the file for, set in the retry's
-- transaction on the job's newest retained file. Returning to the queue
-- under that generation leaves the file to it instead of abandoning it;
-- the attempt adopts it once the coordinator has proved no earlier task of
-- the job still runs, and the adoption clears this. A newer generation
-- that registers its own file, or a restart from zero under another
-- generation, abandons it as usual.
ALTER TABLE staging_files ADD COLUMN handoff_generation INTEGER
    CHECK (handoff_generation > generation);

-- Why the job's latest attempt discarded its saved data and started from
-- zero. Kept until the user retries the job again.
ALTER TABLE jobs ADD COLUMN restart_reason TEXT
    CHECK (length(restart_reason) <= 512);
