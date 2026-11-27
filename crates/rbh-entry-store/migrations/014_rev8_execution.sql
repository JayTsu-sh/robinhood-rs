-- Deleted Rev.8 heads retain their immutable revisions but free the policy name.
ALTER TABLE policies
    MODIFY COLUMN name VARCHAR(255) NULL,
    ADD COLUMN IF NOT EXISTS rev8_next_run DATETIME(6) NULL,
    ADD INDEX IF NOT EXISTS idx_rev8_due (rev8_next_run, id);

CREATE TABLE IF NOT EXISTS rev8_worker_lock (id TINYINT UNSIGNED PRIMARY KEY) ENGINE=InnoDB;
INSERT IGNORE INTO rev8_worker_lock VALUES (1);

CREATE TABLE IF NOT EXISTS rev8_runs (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    policy_id BIGINT UNSIGNED NOT NULL,
    revision BIGINT UNSIGNED NOT NULL,
    state ENUM('queued','running','previewed','finished','deferred','cancelled') NOT NULL DEFAULT 'queued',
    reason VARCHAR(255) NULL,
    candidate_count BIGINT UNSIGNED NULL,
    estimated_bytes BIGINT UNSIGNED NULL,
    cursor_id BINARY(16) NULL,
    requested_by VARCHAR(255) NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    started_at DATETIME(6) NULL,
    updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),
    FOREIGN KEY (policy_id, revision) REFERENCES rev8_policy_revisions (policy_id, revision),
    INDEX idx_rev8_runs_policy (policy_id, id),
    INDEX idx_rev8_runs_queue (state, updated_at, id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS rev8_breakers (
    policy_id BIGINT UNSIGNED PRIMARY KEY,
    opened BOOLEAN NOT NULL DEFAULT FALSE,
    reset_by VARCHAR(255) NULL,
    reset_at DATETIME(6) NULL,
    FOREIGN KEY (policy_id) REFERENCES policies (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS rev8_operations (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    run_id BIGINT UNSIGNED NOT NULL,
    filesystem VARCHAR(64) NOT NULL,
    fid BINARY(16) NOT NULL,
    action VARCHAR(64) NOT NULL,
    archive_id INT UNSIGNED NOT NULL DEFAULT 0,
    state ENUM('submitting','submitted','unknown','completed','failed','deferred') NOT NULL,
    reason VARCHAR(255) NULL,
    execution_profile VARCHAR(253) NULL,
    terminal_error_class VARCHAR(32) NULL,
    finished_at DATETIME(6) NULL,
    completion_evidence JSON NULL,
    attempted BOOLEAN NOT NULL DEFAULT FALSE,
    generation JSON NULL,
    principal JSON NULL,
    size BIGINT UNSIGNED NOT NULL,
    accepted_at DATETIME(6) NULL,
    checked_at DATETIME(6) NULL,
    -- Keep unresolved work exclusive even across policy revisions.
    pending_fid BINARY(16) AS (CASE WHEN state IN ('submitting','submitted','unknown') THEN fid ELSE NULL END) STORED,
    UNIQUE (run_id, fid),
    UNIQUE (filesystem, pending_fid),
    FOREIGN KEY (run_id) REFERENCES rev8_runs (id),
    INDEX idx_rev8_operations_pending (pending_fid, checked_at, id),
    INDEX idx_rev8_profile_pending (execution_profile, pending_fid)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
