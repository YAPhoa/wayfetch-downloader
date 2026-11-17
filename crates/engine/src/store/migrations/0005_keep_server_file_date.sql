-- Whether a finished download's file takes the modification time its
-- server reported (`Last-Modified`); off unless the user turns it on.
ALTER TABLE settings ADD COLUMN keep_server_file_date INTEGER NOT NULL DEFAULT 0
    CHECK (keep_server_file_date IN (0, 1));
