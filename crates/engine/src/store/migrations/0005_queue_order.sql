-- A job's place in the queue: waiting jobs start in ascending order of
-- it, then of ID. 0 until the user moves the job, so earlier databases
-- keep their order; a move to the top or bottom gives the job a place
-- before or after every other job's, which it keeps until moved again.
ALTER TABLE jobs ADD COLUMN queue_rank INTEGER NOT NULL DEFAULT 0;

-- Admission and the queued list read waiting jobs in queue order.
CREATE INDEX jobs_queue ON jobs (stage, queue_rank, id);

-- A move finds the lowest or highest place in one step.
CREATE INDEX jobs_by_queue_rank ON jobs (queue_rank);
