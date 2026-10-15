-- Wayfetch state schema, migration 9: provider accounts and their
-- encrypted credentials (architecture brief sections 6.1, 6.2, 8.4,
-- ADR-24).
--
-- Migrations are append-only once merged: never edit this file, add the
-- next numbered file instead. See the store::schema module.

-- The check of the key that encrypts credentials: an HMAC of a fixed label
-- under it, so a wrong key is recognized without decrypting anything. The
-- key itself is never stored here. One row, written when the first
-- account is added.
CREATE TABLE credential_vault (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    key_check BLOB NOT NULL CHECK (length(key_check) = 32),
    created_at INTEGER NOT NULL
) STRICT;

-- Accounts the user added, with only redacted metadata. `revision` guards
-- the user's edits; `credential_generation` advances whenever the stored
-- credentials change (signed in again, refreshed), so work started with
-- older credentials cannot store its result. Status fields are what the
-- provider last said; unknown values stay NULL. AUTOINCREMENT keeps IDs
-- from being reused after a removal.
CREATE TABLE accounts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    service TEXT NOT NULL CHECK (length(service) BETWEEN 1 AND 64),
    alias TEXT CHECK (alias IS NULL OR length(alias) BETWEEN 1 AND 64),
    revision INTEGER NOT NULL CHECK (revision > 0),
    credential_generation INTEGER NOT NULL CHECK (credential_generation > 0),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    status TEXT NOT NULL CHECK (
        status IN ('not_checked', 'authenticated', 'invalid_credentials', 'unavailable')
    ),
    problem TEXT CHECK (problem IS NULL OR length(problem) <= 512),
    checked_at INTEGER,
    provider_user TEXT CHECK (provider_user IS NULL OR length(provider_user) BETWEEN 1 AND 128),
    username TEXT CHECK (username IS NULL OR length(username) BETWEEN 1 AND 128),
    plan TEXT CHECK (plan IS NULL OR plan IN ('premium', 'free')),
    expires_at INTEGER,
    added_at INTEGER NOT NULL,
    UNIQUE (service, provider_user)
) STRICT;

-- An account's credentials, sealed with the vault's key: a random nonce,
-- then ciphertext and tag. `reference` is the opaque secret reference the
-- seal's associated data binds; a record copied to another row does not
-- open. Bounded in size.
CREATE TABLE account_credentials (
    account_id INTEGER PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    reference BLOB NOT NULL UNIQUE CHECK (length(reference) = 16),
    format INTEGER NOT NULL CHECK (format = 1),
    sealed BLOB NOT NULL CHECK (length(sealed) BETWEEN 28 AND 8220)
) STRICT;
