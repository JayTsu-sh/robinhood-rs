-- Keep old scan snapshots from overwriting an applied LYOUT observation.
ALTER TABLE scoped_entries ADD COLUMN IF NOT EXISTS layout_observed_at BIGINT NULL;
