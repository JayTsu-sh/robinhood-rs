ALTER TABLE policies
    ADD COLUMN rev8_heat_filesystem VARCHAR(64) NULL,
    ADD INDEX idx_rev8_heat_policy (rev8_heat_filesystem, enabled, id);

CREATE TABLE lustre_read_heat (
    filesystem_id VARCHAR(64) NOT NULL,
    object_kind TINYINT UNSIGNED NOT NULL DEFAULT 0,
    object_id BINARY(16) NOT NULL,
    client_id VARCHAR(63) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    profile_hash BINARY(32) NOT NULL,
    observed_at DATETIME(6) NOT NULL,
    read_samples BIGINT UNSIGNED NULL,
    read_bytes BIGINT UNSIGNED NULL,
    period_seconds INT UNSIGNED NULL,
    decay_percentage TINYINT UNSIGNED NULL,
    failure VARCHAR(48) NULL,
    PRIMARY KEY (filesystem_id, object_kind, object_id, client_id),
    CONSTRAINT fk_lustre_read_heat_entry FOREIGN KEY (filesystem_id, object_kind, object_id)
        REFERENCES scoped_entries (filesystem_id, object_kind, object_id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
