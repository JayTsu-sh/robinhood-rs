-- Queryable, filesystem-scoped catalog projection.
--
-- `entry_data` remains the canonical serialization during the expand phase,
-- while these columns make predicates and reports indexable without losing
-- the backend-native composite identity.
ALTER TABLE scoped_entries
    ADD COLUMN IF NOT EXISTS parent_kind TINYINT UNSIGNED NULL,
    ADD COLUMN IF NOT EXISTS parent_id BINARY(16) NULL,
    ADD COLUMN IF NOT EXISTS fid BINARY(16) NULL COMMENT 'Lustre compatibility projection; NULL for JuiceFS',
    ADD COLUMN IF NOT EXISTS name VARBINARY(255) NOT NULL DEFAULT '',
    ADD COLUMN IF NOT EXISTS kind TINYINT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS size BIGINT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS blocks BIGINT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS uid INT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS gid INT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS projid INT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS mode INT UNSIGNED NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS nlink INT UNSIGNED NOT NULL DEFAULT 1,
    ADD COLUMN IF NOT EXISTS atime BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS mtime BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS ctime BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS stripe_count SMALLINT UNSIGNED NULL,
    ADD COLUMN IF NOT EXISTS stripe_size INT UNSIGNED NULL,
    ADD COLUMN IF NOT EXISTS pool_name VARCHAR(64) NULL,
    ADD COLUMN IF NOT EXISTS sm_status JSON NULL,
    ADD COLUMN IF NOT EXISTS last_seen BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS depth INT UNSIGNED NOT NULL DEFAULT 0,
    ADD INDEX IF NOT EXISTS idx_scoped_parent (filesystem_id, parent_kind, parent_id),
    ADD INDEX IF NOT EXISTS idx_scoped_uid (filesystem_id, uid),
    ADD INDEX IF NOT EXISTS idx_scoped_gid (filesystem_id, gid),
    ADD INDEX IF NOT EXISTS idx_scoped_projid (filesystem_id, projid),
    ADD INDEX IF NOT EXISTS idx_scoped_type (filesystem_id, kind),
    ADD INDEX IF NOT EXISTS idx_scoped_mtime (filesystem_id, mtime),
    ADD INDEX IF NOT EXISTS idx_scoped_size (filesystem_id, size),
    ADD INDEX IF NOT EXISTS idx_scoped_last_seen (filesystem_id, last_seen);

-- Backfill rows written by migrations 006/007 before the query projection
-- existed. Namespace parent/object keys remain authoritative in the existing
-- composite key and scoped_namespace_edges tables.
UPDATE scoped_entries SET
    name = CASE WHEN JSON_TYPE(JSON_EXTRACT(entry_data, '$.name')) = 'STRING'
                THEN JSON_UNQUOTE(JSON_EXTRACT(entry_data, '$.name')) ELSE name END,
    kind = CASE JSON_UNQUOTE(JSON_EXTRACT(entry_data, '$.kind'))
             WHEN 'File' THEN 0 WHEN 'Directory' THEN 1 WHEN 'Symlink' THEN 2
             WHEN 'CharDevice' THEN 3 WHEN 'BlockDevice' THEN 4
             WHEN 'Fifo' THEN 5 WHEN 'Socket' THEN 6 ELSE kind END,
    size = COALESCE(JSON_VALUE(entry_data, '$.size'), 0),
    blocks = COALESCE(JSON_VALUE(entry_data, '$.blocks'), 0),
    uid = COALESCE(JSON_VALUE(entry_data, '$.uid'), 0),
    gid = COALESCE(JSON_VALUE(entry_data, '$.gid'), 0),
    projid = COALESCE(JSON_VALUE(entry_data, '$.projid'), 0),
    mode = COALESCE(JSON_VALUE(entry_data, '$.mode'), 0),
    nlink = COALESCE(JSON_VALUE(entry_data, '$.nlink'), 1),
    atime = COALESCE(JSON_VALUE(entry_data, '$.atime'), 0),
    mtime = COALESCE(JSON_VALUE(entry_data, '$.mtime'), 0),
    ctime = COALESCE(JSON_VALUE(entry_data, '$.ctime'), 0),
    stripe_count = JSON_VALUE(entry_data, '$.stripe_count'),
    stripe_size = JSON_VALUE(entry_data, '$.stripe_size'),
    pool_name = JSON_VALUE(entry_data, '$.pool_name'),
    sm_status = JSON_EXTRACT(entry_data, '$.sm_status'),
    last_seen = COALESCE(JSON_VALUE(entry_data, '$.last_seen'), 0),
    depth = COALESCE(JSON_VALUE(entry_data, '$.depth'), 0);

UPDATE scoped_entries SET fid = object_id WHERE object_kind = 0;

UPDATE scoped_entries entry
JOIN scoped_namespace_edges edge
  ON edge.filesystem_id = entry.filesystem_id
 AND edge.object_kind = entry.object_kind
 AND edge.object_id = entry.object_id
SET entry.parent_kind = edge.parent_kind,
    entry.parent_id = edge.parent_id,
    entry.name = edge.name;

CREATE TABLE IF NOT EXISTS scoped_removed_entries (
    filesystem_id VARCHAR(64) NOT NULL,
    object_kind TINYINT UNSIGNED NOT NULL,
    object_id BINARY(16) NOT NULL,
    entry_data JSON NOT NULL,
    rm_time BIGINT NOT NULL,
    PRIMARY KEY (filesystem_id, object_kind, object_id),
    INDEX idx_scoped_removed_time (filesystem_id, rm_time),
    CONSTRAINT fk_scoped_removed_filesystem FOREIGN KEY (filesystem_id)
        REFERENCES filesystems(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS scoped_stripe_items (
    filesystem_id VARCHAR(64) NOT NULL,
    object_kind TINYINT UNSIGNED NOT NULL,
    object_id BINARY(16) NOT NULL,
    stripe_index SMALLINT UNSIGNED NOT NULL,
    ost_index INT UNSIGNED NOT NULL,
    PRIMARY KEY (filesystem_id, object_kind, object_id, stripe_index),
    INDEX idx_scoped_stripe_ost (filesystem_id, ost_index),
    CONSTRAINT fk_scoped_stripe_filesystem FOREIGN KEY (filesystem_id)
        REFERENCES filesystems(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Preserve existing Lustre OST assignments at upgrade. When multiple scoped
-- Lustre filesystems contain the same FID, the legacy row is necessarily
-- ambiguous, so it is copied to each scoped identity; subsequent scans replace
-- it with each filesystem's actual layout.
INSERT IGNORE INTO scoped_stripe_items
    (filesystem_id, object_kind, object_id, stripe_index, ost_index)
SELECT entry.filesystem_id, entry.object_kind, entry.object_id,
       stripe.stripe_index, stripe.ost_index
FROM stripe_items stripe
JOIN scoped_entries entry
  ON entry.object_kind = 0 AND entry.object_id = stripe.fid;
