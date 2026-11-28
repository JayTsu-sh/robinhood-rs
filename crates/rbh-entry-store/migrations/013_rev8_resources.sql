CREATE TABLE IF NOT EXISTS rev8_configuration_lock (
    id TINYINT UNSIGNED PRIMARY KEY
) ENGINE=InnoDB;
INSERT IGNORE INTO rev8_configuration_lock (id) VALUES (1);

CREATE TABLE IF NOT EXISTS rev8_resource_heads (
    kind VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    name VARCHAR(253) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    revision BIGINT UNSIGNED NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (kind, name)
) ENGINE=InnoDB;

CREATE TABLE IF NOT EXISTS rev8_resource_revisions (
    kind VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    name VARCHAR(253) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
    revision BIGINT UNSIGNED NOT NULL,
    definition JSON NULL,
    modified_by VARCHAR(255) NOT NULL,
    PRIMARY KEY (kind, name, revision),
    FOREIGN KEY (kind, name) REFERENCES rev8_resource_heads (kind, name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

ALTER TABLE rev8_policy_revisions
    ADD COLUMN IF NOT EXISTS resource_bindings JSON NULL,
    ADD COLUMN IF NOT EXISTS modified_by VARCHAR(255) NULL;

-- The initial Rev.8 preview only accepted these built-in resources.
UPDATE rev8_policy_revisions
SET resource_bindings = JSON_ARRAY(
    JSON_OBJECT('kind', 'ExecutionProfile', 'name', 'builtin-preview', 'revision', 1),
    JSON_OBJECT('kind', 'AuthorizationPolicy', 'name', 'builtin-preview', 'revision', 1),
    JSON_OBJECT('kind', 'ApprovalPolicy', 'name', 'builtin-preview', 'revision', 1),
    JSON_OBJECT('kind', 'ActionContract', 'name', 'lustre.hsm.archive', 'revision', 1)
)
WHERE resource_bindings IS NULL;
