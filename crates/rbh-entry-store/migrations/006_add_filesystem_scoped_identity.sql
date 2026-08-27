-- Expand the Lustre-only catalog with filesystem-scoped identity.
--
-- Existing entries remain in the legacy FID-keyed tables while callers are
-- migrated incrementally. The mapping table establishes the new composite
-- identity without changing any shipped Lustre CRUD path.

CREATE TABLE IF NOT EXISTS filesystems (
    id             VARCHAR(64)  NOT NULL PRIMARY KEY,
    backend_kind   VARCHAR(16)  NOT NULL,
    mount_path     VARBINARY(4096) NOT NULL,
    capabilities   JSON         NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS scoped_entries (
    filesystem_id  VARCHAR(64) NOT NULL,
    object_kind    TINYINT UNSIGNED NOT NULL COMMENT '0=Lustre FID, 1=JuiceFS inode',
    object_id      BINARY(16) NOT NULL,
    entry_data     JSON NOT NULL COMMENT 'Filesystem-scoped entry payload',
    PRIMARY KEY (filesystem_id, object_kind, object_id),
    CONSTRAINT fk_scoped_entry_filesystem
        FOREIGN KEY (filesystem_id) REFERENCES filesystems(id)
        ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
