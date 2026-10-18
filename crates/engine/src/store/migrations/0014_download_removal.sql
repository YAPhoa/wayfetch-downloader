-- Started links move to Downloads; source identity remains for idempotent starts.
UPDATE source_links SET listed = 0 WHERE EXISTS (SELECT 1 FROM jobs WHERE source_link_id = source_links.id);
-- Explicit destructive authority survives a crash. Records of artifacts remain
-- immutable until every removal is durable; only an explicit retry resumes it.
CREATE TABLE deletions (
    job_id INTEGER PRIMARY KEY REFERENCES jobs(id),
    revision INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32)
) STRICT;
