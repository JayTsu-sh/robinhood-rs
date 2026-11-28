-- Revision is NULL for legacy policies. These remain visible to the existing
-- scheduler until the Rev.8 execution path replaces it.
ALTER TABLE policies ADD COLUMN IF NOT EXISTS revision BIGINT UNSIGNED NULL;

CREATE TABLE IF NOT EXISTS rev8_policy_revisions (
    policy_id BIGINT UNSIGNED NOT NULL,
    revision BIGINT UNSIGNED NOT NULL,
    definition JSON NOT NULL,
    PRIMARY KEY (policy_id, revision),
    CONSTRAINT fk_rev8_policy_revisions_policy FOREIGN KEY (policy_id) REFERENCES policies(id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
