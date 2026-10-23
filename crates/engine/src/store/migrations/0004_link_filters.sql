-- The link filters as JSON (the protocol's `LinkFilters`); NULL until the
-- user saves some, which means on, with none.
ALTER TABLE settings ADD COLUMN link_filters TEXT
    CHECK (link_filters IS NULL OR length(link_filters) <= 4194304);
-- For a link held off the list until its check: how it came, as JSON, so
-- the link filters as they are then can judge it again. Cleared when the
-- link is listed; the link is dropped when a filter leaves it out.
ALTER TABLE source_links ADD COLUMN screen TEXT
    CHECK (screen IS NULL OR (listed = 1 AND length(screen) <= 4194304));
-- For a page that was read: how many links found on it the filters left
-- out, as they were collected or once checked.
ALTER TABLE source_links ADD COLUMN filtered INTEGER NOT NULL DEFAULT 0
    CHECK (filtered >= 0);
