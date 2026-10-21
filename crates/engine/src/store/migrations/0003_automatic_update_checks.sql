-- Whether the engine checks for a newer release on its own; on unless the
-- user turns it off.
ALTER TABLE settings ADD COLUMN automatic_update_checks INTEGER NOT NULL DEFAULT 1
    CHECK (automatic_update_checks IN (0, 1));
