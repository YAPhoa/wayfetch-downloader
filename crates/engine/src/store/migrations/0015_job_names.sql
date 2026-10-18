-- A validated provider/response filename hint, available before publication.
-- Source-path hints remain derived from the durable original source link.
ALTER TABLE jobs ADD COLUMN suggested_name TEXT
    CHECK (suggested_name IS NULL OR length(suggested_name) BETWEEN 1 AND 255);

-- Retain existing response hints without reopening a source or sealed link.
-- Readers still apply the central single-component filename policy.
UPDATE jobs SET suggested_name = (
    SELECT f.suggested_name FROM staging_files f WHERE f.job_id = jobs.id
        AND f.representation_known = 1 AND length(f.suggested_name) BETWEEN 1 AND 255
        ORDER BY f.generation DESC LIMIT 1
);
