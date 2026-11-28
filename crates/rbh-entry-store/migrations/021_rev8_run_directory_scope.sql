-- Keep the directory evidence used for the initial candidate estimate.
ALTER TABLE rev8_runs ADD COLUMN IF NOT EXISTS directory_scope JSON NULL;
