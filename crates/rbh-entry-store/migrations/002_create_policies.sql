-- Policy definitions table (Phase 4).
-- Each row holds a complete PolicyDef as JSON. Triggers inside the
-- definition are reconciled to scheduler-rs schedules by rbh-policy.
CREATE TABLE IF NOT EXISTS policies (
    id          BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    name        VARCHAR(255) NOT NULL UNIQUE,
    kind        VARCHAR(32) NOT NULL,
    definition  JSON NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
                    ON UPDATE CURRENT_TIMESTAMP(6)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE INDEX idx_policies_kind ON policies (kind);
CREATE INDEX idx_policies_enabled ON policies (enabled);
