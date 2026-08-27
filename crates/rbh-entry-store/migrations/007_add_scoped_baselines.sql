CREATE TABLE IF NOT EXISTS scoped_namespace_edges (
    filesystem_id VARCHAR(64) NOT NULL,
    parent_kind TINYINT UNSIGNED NOT NULL,
    parent_id BINARY(16) NOT NULL,
    name VARBINARY(255) NOT NULL,
    object_kind TINYINT UNSIGNED NOT NULL,
    object_id BINARY(16) NOT NULL,
    PRIMARY KEY (filesystem_id, parent_kind, parent_id, name),
    INDEX idx_scoped_edge_object (filesystem_id, object_kind, object_id),
    CONSTRAINT fk_scoped_edge_filesystem FOREIGN KEY (filesystem_id)
        REFERENCES filesystems(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS filesystem_baselines (
    filesystem_id VARCHAR(64) NOT NULL PRIMARY KEY,
    state VARCHAR(16) NOT NULL,
    scan_started_at BIGINT NULL,
    completed_at BIGINT NULL,
    last_version BIGINT UNSIGNED NULL,
    invalid_reason VARCHAR(255) NULL,
    CONSTRAINT fk_baseline_filesystem FOREIGN KEY (filesystem_id)
        REFERENCES filesystems(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
