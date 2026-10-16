-- Wayfetch state schema, migration 12: the user's total download speed
-- limit.
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- Bytes per second across every download. NULL means no saved preference:
-- the operator's configured default, else Unlimited, applies. 0 is a saved
-- Unlimited, distinct from NULL, and is read only by the store, which maps
-- it to `SpeedLimit::Unlimited`; it is never compared as a rate. As for
-- migration 10, a saved value is kept even when a later policy no longer
-- permits it, and the supported range is checked by the engine.
ALTER TABLE settings ADD COLUMN download_speed_limit INTEGER
    CHECK (download_speed_limit IS NULL OR download_speed_limit >= 0);
