//! `PolicyStore` — CRUD for policy rows in `rbh_entries.policies`.

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;

use crate::PolicyError;
use crate::model::{PolicyDef, PolicyRow, TriggerSpec};

/// MariaDB-backed policy store.
#[derive(Debug, Clone)]
pub struct PolicyStore {
    pool: MySqlPool,
}

impl PolicyStore {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }

    /// Run migrations for the `policies` table.
    /// Verify that the `policies` table exists.
    ///
    /// The actual schema migration lives in `rbh-entry-store/migrations/002_create_policies.sql`
    /// and is run by `EntryStore::connect()`. This method validates that the table is present
    /// so callers get a clear error if migrations haven't been run.
    pub async fn check_schema(&self) -> Result<(), PolicyError> {
        sqlx::query("SELECT 1 FROM policies LIMIT 0")
            .execute(&self.pool)
            .await
            .map_err(|e| {
                PolicyError::Migration(format!(
                    "policies table not found — run EntryStore::connect() first: {e}"
                ))
            })?;
        Ok(())
    }

    /// Insert a new policy. Returns the auto-generated id.
    ///
    /// Validates all trigger specifications before persisting.
    #[tracing::instrument(skip(self, def), fields(policy_name = %def.name))]
    pub async fn create(&self, def: &PolicyDef) -> Result<u64, PolicyError> {
        validate_triggers(&def.triggers)?;
        let kind = def.kind.as_str();
        let definition_json = serde_json::to_string(def)?;
        let result = sqlx::query("INSERT INTO policies (name, kind, definition, enabled) VALUES (?, ?, ?, ?)")
            .bind(&def.name)
            .bind(kind)
            .bind(&definition_json)
            .bind(def.enabled)
            .execute(&self.pool)
            .await?;
        Ok(result.last_insert_id())
    }

    /// Fetch a policy by id.
    #[tracing::instrument(skip(self))]
    pub async fn get(&self, id: u64) -> Result<PolicyRow, PolicyError> {
        let row = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at \
             FROM policies WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(PolicyError::NotFound(id))?;
        row.into_policy_row()
    }

    /// Fetch a policy by name.
    #[tracing::instrument(skip(self))]
    pub async fn get_by_name(&self, name: &str) -> Result<PolicyRow, PolicyError> {
        let row = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at \
             FROM policies WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| PolicyError::NotFoundByName(name.to_string()))?;
        row.into_policy_row()
    }

    /// List all policies.
    #[tracing::instrument(skip(self))]
    pub async fn list(&self) -> Result<Vec<PolicyRow>, PolicyError> {
        let rows = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at \
             FROM policies ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| r.into_policy_row()).collect()
    }

    /// Replace a policy definition (full PUT semantics).
    ///
    /// Validates all trigger specifications before persisting.
    #[tracing::instrument(skip(self, def), fields(policy_name = %def.name))]
    pub async fn update(&self, id: u64, def: &PolicyDef) -> Result<(), PolicyError> {
        validate_triggers(&def.triggers)?;
        let kind = def.kind.as_str();
        let definition_json = serde_json::to_string(def)?;
        let result = sqlx::query("UPDATE policies SET name = ?, kind = ?, definition = ?, enabled = ? WHERE id = ?")
            .bind(&def.name)
            .bind(kind)
            .bind(&definition_json)
            .bind(def.enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }

    /// Delete a policy by id.
    #[tracing::instrument(skip(self))]
    pub async fn delete(&self, id: u64) -> Result<(), PolicyError> {
        let result = sqlx::query("DELETE FROM policies WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }
}

/// Validate all trigger specs eagerly so invalid cron expressions etc.
/// are rejected at creation time, not at reconciliation time.
fn validate_triggers(triggers: &[TriggerSpec]) -> Result<(), PolicyError> {
    use crate::model::WindowModeSpec;
    for (i, spec) in triggers.iter().enumerate() {
        match spec {
            TriggerSpec::Cron { expression } => {
                scheduler_rs::trigger::CronTrigger::new(expression)
                    .map_err(|e| PolicyError::InvalidTrigger(format!("trigger[{i}]: invalid cron: {e}")))?;
            }
            TriggerSpec::Interval { secs } if *secs == 0 => {
                return Err(PolicyError::InvalidTrigger(format!(
                    "trigger[{i}]: interval must be > 0 seconds"
                )));
            }
            TriggerSpec::Window {
                mode: WindowModeSpec::Repeat { interval_secs },
                ..
            } if *interval_secs == 0 => {
                return Err(PolicyError::InvalidTrigger(format!(
                    "trigger[{i}]: window repeat interval must be > 0 seconds"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Raw row from sqlx — definition is a JSON string that we parse.
#[derive(Debug, sqlx::FromRow)]
struct RawPolicyRow {
    id: u64,
    name: String,
    /// MariaDB returns JSON columns as BLOB — decode via `Vec<u8>`.
    definition: Vec<u8>,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl RawPolicyRow {
    fn into_policy_row(self) -> Result<PolicyRow, PolicyError> {
        let definition: PolicyDef = serde_json::from_slice(&self.definition)?;
        Ok(PolicyRow {
            id: self.id,
            name: self.name,
            definition,
            enabled: self.enabled,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}
