-- Whether the engine checks for a newer release on its own; on unless the
-- user turns it off.
ALTER TABLE settings ADD COLUMN automatic_updates INTEGER NOT NULL DEFAULT 1
    CHECK (automatic_updates IN (0, 1));
