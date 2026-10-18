-- Media links (protocol 4.0): a collected link a media service recognized,
-- what checking it found, and what it will be downloaded as.
ALTER TABLE source_links ADD COLUMN media_service TEXT
    CHECK (media_service IS NULL OR length(media_service) BETWEEN 1 AND 64);
ALTER TABLE source_links ADD COLUMN media_state TEXT
    CHECK (media_state IN ('pending', 'ready', 'failed'));
-- The service's answer as JSON (title, duration, heights); bounded.
ALTER TABLE source_links ADD COLUMN media_info TEXT
    CHECK (media_info IS NULL OR length(media_info) <= 8192);
ALTER TABLE source_links ADD COLUMN media_problem TEXT;
-- The person's choice as JSON; absent means the default (best video).
ALTER TABLE source_links ADD COLUMN media_choice TEXT
    CHECK (media_choice IS NULL OR length(media_choice) <= 256);

CREATE INDEX source_links_media_pending ON source_links (id)
    WHERE media_state = 'pending' AND listed = 1;

-- A download of a media link keeps its service and choice.
ALTER TABLE jobs ADD COLUMN media_service TEXT
    CHECK (media_service IS NULL OR length(media_service) BETWEEN 1 AND 64);
ALTER TABLE jobs ADD COLUMN media_choice TEXT
    CHECK (media_choice IS NULL OR length(media_choice) <= 256);

-- A media download's streams are joined or converted into a second file
-- in the same staging folder. While converting, `aux_*` names the output
-- being written; once converted, the output becomes the staging file and
-- `aux_*` names the downloaded streams until they are removed. Every file
-- stays named by a record, so cleanup can remove it by identity.
ALTER TABLE staging_files ADD COLUMN media_phase TEXT
    CHECK (media_phase IN ('converting', 'converted'));
ALTER TABLE staging_files ADD COLUMN aux_name TEXT
    CHECK (aux_name IS NULL OR length(aux_name) BETWEEN 1 AND 255);
ALTER TABLE staging_files ADD COLUMN aux_device INTEGER;
ALTER TABLE staging_files ADD COLUMN aux_inode INTEGER;
ALTER TABLE staging_files ADD COLUMN aux_birth_ns INTEGER;
