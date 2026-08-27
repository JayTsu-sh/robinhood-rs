-- Existing policy JSON predates filesystem-scoped evaluation. Preserve the
-- legacy single-Lustre behavior explicitly before PolicyDef makes the field
-- required for every new configuration. The daemon binds this marker to its
-- configured (and uniquely selected) Lustre filesystem before loading policy
-- definitions, so custom RBH_FILESYSTEM_ID values remain compatible.
UPDATE policies
SET definition = JSON_SET(definition, '$.filesystem', '__legacy_lustre__')
WHERE JSON_EXTRACT(definition, '$.filesystem') IS NULL;
