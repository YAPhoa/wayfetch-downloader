-- Wayfetch state schema, migration 9: engine preferences the user changes
-- at run time.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- The user's saved preferences, one typed column each, on the settings
-- row so a change is revision-checked with the default destination. NULL
-- means no saved preference: the operator's configured default, else the
-- built-in one, applies. A saved value is kept even when a later policy no
-- longer permits it; the engine then runs within the policy and reports
-- the conflict. The supported range is checked by the engine, so it can
-- change without a migration.
ALTER TABLE settings ADD COLUMN max_active_downloads INTEGER
    CHECK (max_active_downloads IS NULL OR max_active_downloads > 0);
