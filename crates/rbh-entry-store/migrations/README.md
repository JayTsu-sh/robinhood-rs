# rbh_entries migrations

This directory is consumed by `sqlx::migrate!("./migrations")` in
`EntryStore::connect()`. The file-naming contract:

```
NNN_short_description.sql
```

* `NNN` is a monotonically increasing zero-padded version integer.
* Each `.sql` is executed exactly once per database; success is
  recorded in the `_sqlx_migrations` table.
* Migrations are applied in ascending `NNN` order at daemon startup
  (and from every `rbh-entry-store` test that connects). sqlx wraps
  each file in a transaction by default.

## Conventions

1. **Never modify an already-shipped file.** Operators who upgraded
   between versions won't re-run it. Add a new `NNN+1` file instead.
2. **Wrap destructive changes in `IF EXISTS` / `IF NOT EXISTS`.**
   sqlx doesn't retry on partial failure; an idempotent file lets
   operators re-run a migration manually if something went wrong.
3. **MariaDB 11 is the baseline.** Generated columns, JSON functions,
   and `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` are available.
4. **Test additive changes locally** with a fresh DB before merging.
   A clean first-time install must succeed; an upgrade from the
   previous version must also succeed.

## Current migrations

| File | Purpose |
|------|---------|
| `001_initial_schema.sql` | entries, names, stripe_items, removed_entries, changelog_cursor |
| `002_create_policies.sql` | policies table (JSON PolicyDef payload) |
| `003_hsm_state_indexes.sql` | virtual column + index for `hsm_state`, index on `atime` / `mtime` |
| `004_add_depth_column.sql` | directory depth for scan and subtree queries |
| `005_create_classifiers.sql` | classifier definitions stored as JSON |
| `006_add_filesystem_scoped_identity.sql` | filesystem registry and backend-native scoped catalog entries |
| `007_add_scoped_baselines.sql` | filesystem baseline state and parent/name namespace edges |
| `008_scope_catalog_consumers.sql` | queryable filesystem-scoped metadata, removed entries, and stripe relationships |
| `009_scope_policies.sql` | bind legacy policy definitions explicitly to the `lustre` filesystem |
