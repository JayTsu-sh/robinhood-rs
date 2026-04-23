-- Performance indexes for HSM state queries and recency filters.
--
-- `rbh find --hsm-state released` pushes down to
-- JSON_UNQUOTE(JSON_EXTRACT(sm_status, '$.hsm_state')) = ?, which
-- without a helper column is a full table scan. A generated column
-- with an index gives constant-time filtering while leaving the
-- canonical data in sm_status untouched.
--
-- MariaDB 10.2+ / MySQL 5.7+ supports generated columns; robinhood-rs
-- requires MariaDB 11 in the dev compose, so this is safe.

ALTER TABLE entries
    ADD COLUMN hsm_state VARCHAR(16) AS (
        JSON_UNQUOTE(JSON_EXTRACT(sm_status, '$.hsm_state'))
    ) VIRTUAL;

ALTER TABLE entries
    ADD INDEX idx_entries_hsm_state (hsm_state);

-- Index for the rbh_find / rbh_report `oldest` query path (ORDER BY
-- atime ASC LIMIT N). `mtime`, `size`, `uid`, `gid` already have
-- indexes from 001_initial_schema; atime is the one missing.
ALTER TABLE entries
    ADD INDEX idx_entries_atime (atime);
