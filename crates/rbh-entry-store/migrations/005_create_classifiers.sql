-- Classifier registry: named tag-assignment rules driven by the Classification Engine.
-- Each row is a ClassifierDef JSON blob. Tags written to entries.sm_status.xattr.*.
CREATE TABLE IF NOT EXISTS classifiers (
    id          BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    name        VARCHAR(255) NOT NULL UNIQUE,
    definition  JSON NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    updated_at  DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
                    ON UPDATE CURRENT_TIMESTAMP(6)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE INDEX idx_classifiers_enabled ON classifiers (enabled);
