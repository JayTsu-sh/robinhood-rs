-- Add depth column to entries table.
-- Depth 0 = filesystem root, 1 = direct child of root, etc.
-- DEFAULT 0 ensures existing rows remain queryable without re-scan.
ALTER TABLE entries
    ADD COLUMN depth INT UNSIGNED NOT NULL DEFAULT 0 AFTER last_seen,
    ADD INDEX idx_depth (depth);
