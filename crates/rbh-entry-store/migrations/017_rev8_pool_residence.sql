ALTER TABLE scoped_entries ADD COLUMN IF NOT EXISTS pool_entered_at BIGINT NULL;

-- Start the residence clock at upgrade for previously observed pools.
UPDATE scoped_entries
SET pool_entered_at = TIMESTAMPDIFF(MICROSECOND, '1970-01-01', UTC_TIMESTAMP(6))
WHERE pool_entered_at IS NULL AND object_kind = 0 AND pool_name IS NOT NULL AND layout_observed_at IS NOT NULL;

ALTER TABLE rev8_runs ADD COLUMN IF NOT EXISTS heat_deferred_by_reason JSON NULL;
