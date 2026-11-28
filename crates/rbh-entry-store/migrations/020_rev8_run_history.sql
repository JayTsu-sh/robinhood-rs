ALTER TABLE rev8_runs
    ADD INDEX IF NOT EXISTS idx_rev8_runs_state_id (state, id),
    ADD INDEX IF NOT EXISTS idx_rev8_runs_policy_state_id (policy_id, state, id);

ALTER TABLE rev8_operations
    ADD INDEX IF NOT EXISTS idx_rev8_operations_run_id (run_id, id),
    ADD INDEX IF NOT EXISTS idx_rev8_operations_run_state_id (run_id, state, id);
