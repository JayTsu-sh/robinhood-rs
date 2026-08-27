-- A given MDT name is only unique within a Lustre filesystem. Keep legacy
-- cursor rows under an explicit compatibility scope while making all new
-- runtime cursors filesystem-scoped.
ALTER TABLE changelog_cursor
    ADD COLUMN IF NOT EXISTS filesystem_id VARCHAR(64) NOT NULL DEFAULT '__legacy_lustre__' FIRST;

-- MariaDB has no `DROP PRIMARY KEY IF EXISTS`. Use metadata-driven dynamic
-- DDL so a retry is safe after either half of this migration committed.
SET @rbh_cursor_has_primary = (
    SELECT COUNT(*) FROM information_schema.table_constraints
    WHERE table_schema = DATABASE()
      AND table_name = 'changelog_cursor'
      AND constraint_type = 'PRIMARY KEY'
);
SET @rbh_cursor_drop_primary = IF(
    @rbh_cursor_has_primary > 0,
    'ALTER TABLE changelog_cursor DROP PRIMARY KEY',
    'SELECT 1'
);
PREPARE rbh_cursor_stmt FROM @rbh_cursor_drop_primary;
EXECUTE rbh_cursor_stmt;
DEALLOCATE PREPARE rbh_cursor_stmt;

CREATE UNIQUE INDEX IF NOT EXISTS uq_changelog_cursor_filesystem_mdt
    ON changelog_cursor (filesystem_id, mdt_name);
