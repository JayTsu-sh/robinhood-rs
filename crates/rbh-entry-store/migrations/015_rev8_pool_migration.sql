-- A pending layout operation owns both the FID and its filesystem slot.
ALTER TABLE rev8_operations
    ADD COLUMN IF NOT EXISTS migration_plan JSON NULL,
    ADD COLUMN IF NOT EXISTS pending_migration_fs VARCHAR(64) AS (
        CASE WHEN migration_plan IS NOT NULL AND state IN ('submitting','submitted','unknown')
            THEN filesystem ELSE NULL END
    ) STORED,
    ADD UNIQUE INDEX IF NOT EXISTS idx_rev8_migration_fs (pending_migration_fs);
