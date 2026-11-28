use sqlx::{MySql, Pool};

use crate::error::{Result, StoreError};

const SCHEMA_VERSION: u32 = 1;

const REQUIRED_TABLES: &[&str] = &[
    "changelog_cursor",
    "classifiers",
    "entries",
    "filesystem_baselines",
    "filesystem_resource_snapshots",
    "filesystems",
    "lustre_read_heat",
    "names",
    "policies",
    "policy_breakers",
    "policy_configuration_lock",
    "policy_operations",
    "policy_resource_heads",
    "policy_resource_revisions",
    "policy_revisions",
    "policy_runs",
    "policy_worker_lock",
    "removed_entries",
    "scoped_entries",
    "scoped_namespace_edges",
    "scoped_removed_entries",
    "scoped_stripe_items",
    "stripe_items",
];

pub(crate) async fn validate(pool: &Pool<MySql>) -> Result<()> {
    let version = sqlx::query_scalar::<_, u32>("SELECT version FROM rbh_schema WHERE id = 1")
        .fetch_optional(pool)
        .await;
    let actual = match version {
        Ok(Some(version)) => version,
        Ok(None) => return Err(StoreError::SchemaMissing),
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42S02") => {
            return Err(StoreError::SchemaMissing);
        }
        Err(error) => return Err(error.into()),
    };
    if actual != SCHEMA_VERSION {
        return Err(StoreError::SchemaVersion {
            actual,
            expected: SCHEMA_VERSION,
        });
    }

    let tables = sqlx::query_scalar::<_, String>(
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND ENGINE = 'InnoDB'",
    )
    .fetch_all(pool)
    .await?;
    for &table in REQUIRED_TABLES {
        if !tables.iter().any(|name| name == table) {
            return Err(StoreError::SchemaTableMissing(table));
        }
    }

    let (configuration, worker): (bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM policy_configuration_lock WHERE id = 1),
                EXISTS(SELECT 1 FROM policy_worker_lock WHERE id = 1)",
    )
    .fetch_one(pool)
    .await?;
    if !configuration {
        return Err(StoreError::SchemaLockMissing("policy_configuration_lock"));
    }
    if !worker {
        return Err(StoreError::SchemaLockMissing("policy_worker_lock"));
    }
    Ok(())
}
