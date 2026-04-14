-- robinhood-rs entry catalog schema.
-- Phase 2: core tables for changelog ingestion + fs-scan.
-- Incorporates index optimizations from robinhood-C's list_mgr (listmgr_init.c).
-- Policy tables (policies, policy_trigger_state, etc.) are added in Phase 6+.

-- Main entry catalog. Primary key is the Lustre FID packed as BINARY(16).
-- See entry_row_replaces_nasentry.md for the design rationale.
CREATE TABLE IF NOT EXISTS entries (
    fid            BINARY(16)    NOT NULL PRIMARY KEY,
    parent_fid     BINARY(16),
    name           VARBINARY(255),
    kind           TINYINT UNSIGNED NOT NULL COMMENT '0=file,1=dir,2=symlink,3=chardev,4=blockdev,5=fifo,6=socket',
    size           BIGINT UNSIGNED NOT NULL DEFAULT 0,
    blocks         BIGINT UNSIGNED NOT NULL DEFAULT 0,
    uid            INT UNSIGNED    NOT NULL DEFAULT 0,
    gid            INT UNSIGNED    NOT NULL DEFAULT 0,
    projid         INT UNSIGNED    NOT NULL DEFAULT 0,
    mode           INT UNSIGNED    NOT NULL DEFAULT 0,
    nlink          INT UNSIGNED    NOT NULL DEFAULT 1,
    atime          BIGINT          NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
    mtime          BIGINT          NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
    ctime          BIGINT          NOT NULL DEFAULT 0 COMMENT 'UNIX seconds',
    stripe_count   SMALLINT UNSIGNED,
    stripe_size    INT UNSIGNED,
    pool_name      VARCHAR(64),
    sm_status      JSON            COMMENT 'Per-status-manager state keyed by SM name',
    last_seen      BIGINT          NOT NULL DEFAULT 0 COMMENT 'UNIX seconds of last observation',

    -- Indexes mirror robinhood-C's per-field indexes from listmgr_init.c.
    -- These are the fields most commonly used in policy predicates.
    INDEX idx_parent   (parent_fid),
    INDEX idx_uid      (uid),
    INDEX idx_gid      (gid),
    INDEX idx_projid   (projid),
    INDEX idx_type     (kind),
    INDEX idx_mtime    (mtime),
    INDEX idx_size     (size),
    INDEX idx_last_seen(last_seen)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Hardlink support: one row per (fid, parent_fid, name) triple.
-- A file with N hardlinks has N rows in `names` and 1 row in `entries`.
-- Robinhood-C's DNAMES_TABLE uses a packed key (pkn) for fast directory-scoped
-- lookups; we use a composite PK (fid, parent_fid, name) and an additional
-- index on (parent_fid, name) for "resolve name in directory" queries.
CREATE TABLE IF NOT EXISTS names (
    fid        BINARY(16)    NOT NULL,
    parent_fid BINARY(16)    NOT NULL,
    name       VARBINARY(255) NOT NULL,
    PRIMARY KEY (fid, parent_fid, name),
    INDEX idx_names_parent      (parent_fid),
    INDEX idx_names_parent_name (parent_fid, name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Per-stripe OST assignment. Robinhood-C's STRIPE_ITEMS_TABLE tracks which
-- OSTs hold each file's stripes, enabling "which files are on OST X" queries
-- for targeted migration/purge policies.
CREATE TABLE IF NOT EXISTS stripe_items (
    fid          BINARY(16)       NOT NULL,
    stripe_index SMALLINT UNSIGNED NOT NULL COMMENT 'Stripe number within the file',
    ost_index    INT UNSIGNED      NOT NULL COMMENT 'OST index holding this stripe',
    PRIMARY KEY (fid, stripe_index),
    INDEX idx_stripe_ost (ost_index)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Removed entries: persists deleted files so HSM cleanup policies
-- (e.g. lhsm_remove running on rm_time >= 30d) can act on them.
-- Populated from UNLNK changelog events when last_link=true.
CREATE TABLE IF NOT EXISTS removed_entries (
    fid        BINARY(16)    NOT NULL PRIMARY KEY,
    parent_fid BINARY(16),
    name       VARBINARY(255),
    kind       TINYINT UNSIGNED NOT NULL,
    size       BIGINT UNSIGNED NOT NULL DEFAULT 0,
    uid        INT UNSIGNED    NOT NULL DEFAULT 0,
    gid        INT UNSIGNED    NOT NULL DEFAULT 0,
    sm_status  JSON,
    rm_time    BIGINT          NOT NULL COMMENT 'UNIX seconds when UNLNK was observed',

    INDEX idx_rm_time (rm_time)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- Per-MDT changelog cursor. Implements lustre_changelog::CursorStore.
-- One row per MDT; updated periodically by the listener.
CREATE TABLE IF NOT EXISTS changelog_cursor (
    mdt_name   VARCHAR(64) NOT NULL PRIMARY KEY,
    reader_id  VARCHAR(16) COMMENT 'e.g. cl3 — the pre-registered changelog user',
    last_rec   BIGINT UNSIGNED NOT NULL DEFAULT 0 COMMENT 'Last durably committed record index',
    updated_at BIGINT NOT NULL DEFAULT 0 COMMENT 'UNIX seconds'
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
