-- Wayfetch state schema, migration 4: the origin that served a staging
-- file's representation (rule 7.5.5, T06).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- Scheme, host, and port of the response that started the file. Validators
-- are compared only within one origin, so a matching entity tag from
-- another server never makes its bytes interchangeable. NULL if the
-- transport did not report it.
ALTER TABLE staging_files ADD COLUMN representation_origin TEXT
    CHECK (length(representation_origin) <= 512);
