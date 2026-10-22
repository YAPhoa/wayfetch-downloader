-- The link filters as JSON (the protocol's `LinkFilters`); NULL until the
-- user saves some, which means on, with none.
ALTER TABLE settings ADD COLUMN link_filters TEXT
    CHECK (link_filters IS NULL OR length(link_filters) <= 4194304);
-- For a link held off the list until its check: the conditions of link
-- filters that only the check tells, as JSON. Cleared when none holds and
-- the link is listed; the link is dropped when one does.
ALTER TABLE source_links ADD COLUMN screen TEXT
    CHECK (screen IS NULL OR (listed = 1 AND length(screen) <= 4194304));
