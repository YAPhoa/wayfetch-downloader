-- Wayfetch state schema, migration 8: links collected for review before
-- they are started.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- `listed` is 1 while a collected link is in the Links list, from
-- collecting until the user removes it; a started link stays listed with
-- its job. `held` is 1 while it waits to be started; the ingestion step
-- leaves it alone, and starting it creates its job and clears the flag in
-- one transaction. Links submitted to start at once are never listed.
-- `revision` changes when the user edits the link, so a command made from
-- an older view is refused. `root_id` is where it will be saved, copied to
-- its job; NULL for the operator's download root. It is looked up, not
-- enforced: a folder may stop being permitted while links name it, and
-- its `roots` row, which is never deleted, still gives its path. An
-- entry that is not a valid link is kept with `invalid` = 1 and stored without credentials,
-- and `problem` says why it is invalid or why starting it was last
-- refused.
ALTER TABLE source_links ADD COLUMN held INTEGER NOT NULL DEFAULT 0 CHECK (held IN (0, 1));
ALTER TABLE source_links ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0);
ALTER TABLE source_links ADD COLUMN root_id INTEGER CHECK (root_id > 0);
ALTER TABLE source_links ADD COLUMN invalid INTEGER NOT NULL DEFAULT 0
    CHECK (invalid IN (0, 1) AND (invalid = 0 OR held = 1));
ALTER TABLE source_links ADD COLUMN problem TEXT CHECK (length(problem) <= 512);
ALTER TABLE source_links ADD COLUMN listed INTEGER NOT NULL DEFAULT 0
    CHECK (listed IN (0, 1) AND (held = 0 OR listed = 1));

CREATE INDEX source_links_listed ON source_links (id) WHERE listed = 1;
