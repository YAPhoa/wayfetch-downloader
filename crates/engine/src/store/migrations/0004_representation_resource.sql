-- Wayfetch state schema, migration 4: the resource that served a staging
-- file's representation (rule 7.5.5, T06).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- An opaque identity of the resource whose response started the file, as
-- the transport reports it (the HTTP transport records a digest of the
-- final URL, never the URL, which may carry a signed token). Validators
-- identify representations of one resource only, so a matching entity tag
-- from another resource never makes its bytes interchangeable. NULL if the
-- transport did not report it.
ALTER TABLE staging_files ADD COLUMN representation_resource TEXT
    CHECK (length(representation_resource) <= 512);
