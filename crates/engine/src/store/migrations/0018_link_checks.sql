-- Wayfetch state schema, migration 18: link checks and the links found
-- on pages (protocol 5.0).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- How far a batch's links are looked into: 'analyze' checks every link and
-- reads a typed link that is a web page; 'supported_only' checks only
-- links that look like files or videos. Batches collected before are
-- 'supported_only': their links were never checked, and stay unchecked
-- until the person asks.
ALTER TABLE input_batches ADD COLUMN mode TEXT NOT NULL DEFAULT 'supported_only'
    CHECK (mode IN ('analyze', 'supported_only'));

-- The page a link was found on, NULL for a link the person typed or
-- pasted. A page with links found on it is never deleted while they are,
-- only unlisted, so every link keeps where it came from.
ALTER TABLE source_links ADD COLUMN parent_id INTEGER REFERENCES source_links (id);
-- 1 if the link may be read as a page when it is one: a link the person
-- typed in a batch that analyzes, or one they asked to analyze. A link
-- found on a page never may, so analysis never spiders.
ALTER TABLE source_links ADD COLUMN may_analyze INTEGER NOT NULL DEFAULT 0
    CHECK (may_analyze IN (0, 1));
-- 1 if requests for the link, its checks and its download, may reach only
-- public addresses: a link found on a page that was not itself served from
-- a local address. The page's author chose it, not the person, so it never
-- gets the local networks the operator allows for intentional downloads.
ALTER TABLE source_links ADD COLUMN public_only INTEGER NOT NULL DEFAULT 0
    CHECK (public_only IN (0, 1));
-- What the last finished check found: NULL before any. A media link's
-- check keeps its own columns (migration 16) instead.
ALTER TABLE source_links ADD COLUMN check_state TEXT
    CHECK (check_state IN ('online', 'offline', 'unknown', 'page'));
ALTER TABLE source_links ADD COLUMN check_reason TEXT
    CHECK (check_reason IS NULL OR length(check_reason) BETWEEN 1 AND 32);
ALTER TABLE source_links ADD COLUMN check_problem TEXT
    CHECK (check_problem IS NULL OR length(check_problem) <= 512);
-- A file's size and suggested name, when its server said. NULL size is
-- unknown, never zero.
ALTER TABLE source_links ADD COLUMN size INTEGER CHECK (size >= 0);
ALTER TABLE source_links ADD COLUMN observed_name TEXT
    CHECK (observed_name IS NULL OR length(observed_name) BETWEEN 1 AND 255);
-- For a page that was read: links on it collected, and links to other
-- pages left alone. NULL until it is read.
ALTER TABLE source_links ADD COLUMN found INTEGER CHECK (found >= 0);
ALTER TABLE source_links ADD COLUMN skipped INTEGER CHECK (skipped >= 0);

CREATE INDEX source_links_by_parent ON source_links (parent_id) WHERE parent_id IS NOT NULL;
-- Finds a link already collected in a batch, so one found on two pages is
-- listed once.
CREATE INDEX source_links_by_batch_url ON source_links (batch_id, input_url);

-- The frontier: checks waiting or running, one per link. A row is the
-- check's identity: checking again replaces it, so the old check's late
-- answer finds no row and is dropped. A finished check deletes its row and
-- records what it found on the link in the same transaction. `queue` is
-- what checks are limited by, one at a time each: the link's host, or
-- 'media:' and its video service. Running rows are pending again after a
-- restart; a check only sends HEAD and GET, which may be repeated.
CREATE TABLE crawl_tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_link_id INTEGER NOT NULL UNIQUE REFERENCES source_links (id) ON DELETE CASCADE,
    queue TEXT NOT NULL CHECK (length(queue) BETWEEN 1 AND 320),
    state TEXT NOT NULL CHECK (state IN ('pending', 'running')),
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX crawl_tasks_pending ON crawl_tasks (queue, id) WHERE state = 'pending';

-- Media links waiting for their check move onto the frontier.
INSERT INTO crawl_tasks (source_link_id, queue, state, created_at)
SELECT id, 'media:' || media_service, 'pending', 0 FROM source_links
WHERE media_state = 'pending' AND listed = 1;
