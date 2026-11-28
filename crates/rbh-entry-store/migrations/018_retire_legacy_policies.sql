-- Retain the submitted definitions for operator reference while releasing
-- their names for Rev.8 submissions and disabling their unversioned heads.
UPDATE policies
SET enabled = FALSE, name = NULL, rev8_next_run = NULL
WHERE revision IS NULL AND (enabled OR name IS NOT NULL OR rev8_next_run IS NOT NULL);

-- Older Rev.8 versions indexed native HSM previews that referenced heat.
-- Only heat-dependent tiering policies activate a read-heat sampling job.
UPDATE policies AS policy
JOIN rev8_policy_revisions AS revision
    ON revision.policy_id = policy.id AND revision.revision = policy.revision
SET policy.rev8_heat_filesystem = NULL
WHERE policy.rev8_heat_filesystem IS NOT NULL
  AND COALESCE(JSON_CONTAINS(revision.resource_bindings, '{"kind":"ActionContract","name":"lustre.pool_migrate"}'), 0) = 0;
