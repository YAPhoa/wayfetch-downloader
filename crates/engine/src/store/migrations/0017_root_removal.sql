-- Wayfetch state schema, migration 17: the user may remove any folder
-- that is not the default, the operator's configured ones included.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- `removed` is 1 for a root the user removed: it is not permitted even
-- while the operator still configures its path, until the user adds the
-- folder again. A removed root is never `registered`.
ALTER TABLE roots ADD COLUMN removed INTEGER NOT NULL DEFAULT 0
    CHECK (removed IN (0, 1) AND NOT (removed = 1 AND registered = 1));
