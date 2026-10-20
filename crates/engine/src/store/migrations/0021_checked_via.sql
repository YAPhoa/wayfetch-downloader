-- Wayfetch state schema, migration 21: which provider checked a link,
-- and what its own server said (protocol 6.1).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- The service whose provider answered the link's last check, for a link
-- an enabled account would download; NULL when its own server did.
ALTER TABLE source_links ADD COLUMN checked_via TEXT
    CHECK (checked_via IS NULL OR length(checked_via) BETWEEN 1 AND 64);
-- 1 if the link's own server last answered with a web page, kept apart
-- from a provider's answer shown instead: starting it directly downloads
-- the page, and is refused.
ALTER TABLE source_links ADD COLUMN origin_page INTEGER NOT NULL DEFAULT 0
    CHECK (origin_page IN (0, 1));
