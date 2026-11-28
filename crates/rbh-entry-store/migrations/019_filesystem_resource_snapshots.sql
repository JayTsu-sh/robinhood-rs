-- Keep the latest console observation and the complete capacity samples used
-- as a fallback when a later collection is incomplete.
CREATE TABLE IF NOT EXISTS filesystem_resource_snapshots (
    observer_id   VARBINARY(255) NOT NULL,
    filesystem_id VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    revision      BIGINT UNSIGNED NOT NULL DEFAULT 1,
    observed_at   BIGINT UNSIGNED NOT NULL,
    observation   JSON NOT NULL,
    PRIMARY KEY (observer_id, filesystem_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
