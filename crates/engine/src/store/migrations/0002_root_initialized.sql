-- 1 once a root's marker and private folders were all written. A root is
-- recorded before that (its ID is allocated before its marker, and a marker
-- left by a failed attempt is adopted), so this tells a setup that never
-- finished, which is retried, from a finished one, which is never set up
-- again: an empty folder in its place is a missing mount (ADR-29). Roots
-- recorded before this column were in use, so they count as initialized.
ALTER TABLE roots ADD COLUMN initialized INTEGER NOT NULL DEFAULT 1
    CHECK (initialized IN (0, 1));
