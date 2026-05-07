//! `PolicyStore` — CRUD for action policy rows in `rbh_entries.policies`.

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;

use crate::PolicyError;
use crate::model::{PolicyDef, PolicyRow};
use crate::trigger_parser::parse_trigger;

/// MariaDB-backed action policy store.
#[derive(Debug, Clone)]
pub struct PolicyStore {
    pool: MySqlPool,
}

impl PolicyStore {
    pub fn new(pool: MySqlPool) -> Self {
        Self { pool }
    }

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

    #[tracing::instrument(skip(self, def), fields(policy_name = %def.name))]
    pub async fn create(&self, def: &PolicyDef) -> Result<u64, PolicyError> {
        validate_policy(def)?;
        let kind = def.kind.as_str();
        let json = serde_json::to_string(def)?;
        let result = sqlx::query("INSERT INTO policies (name, kind, definition, enabled) VALUES (?, ?, ?, ?)")
            .bind(&def.name)
            .bind(kind)
            .bind(&json)
            .bind(def.enabled)
            .execute(&self.pool)
            .await?;
        Ok(result.last_insert_id())
    }

    #[tracing::instrument(skip(self))]
    pub async fn get(&self, id: u64) -> Result<PolicyRow, PolicyError> {
        let row = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at FROM policies WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(PolicyError::NotFound(id))?;
        row.into_policy_row()
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_by_name(&self, name: &str) -> Result<PolicyRow, PolicyError> {
        let row = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at FROM policies WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| PolicyError::NotFoundByName(name.to_string()))?;
        row.into_policy_row()
    }

    #[tracing::instrument(skip(self))]
    pub async fn list(&self) -> Result<Vec<PolicyRow>, PolicyError> {
        let rows = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at FROM policies ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(|r| r.into_policy_row()).collect()
    }

    #[tracing::instrument(skip(self, def), fields(policy_name = %def.name))]
    pub async fn update(&self, id: u64, def: &PolicyDef) -> Result<(), PolicyError> {
        validate_policy(def)?;
        let kind = def.kind.as_str();
        let json = serde_json::to_string(def)?;
        let result = sqlx::query("UPDATE policies SET name = ?, kind = ?, definition = ?, enabled = ? WHERE id = ?")
            .bind(&def.name)
            .bind(kind)
            .bind(&json)
            .bind(def.enabled)
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }

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

/// Validate a PolicyDef before persisting.
fn validate_policy(def: &PolicyDef) -> Result<(), PolicyError> {
    if def.name.is_empty() {
        return Err(PolicyError::InvalidTrigger("policy name must not be empty".into()));
    }
    // Validate trigger string by parsing it
    let spec = parse_trigger(&def.trigger)
        .map_err(|e| PolicyError::InvalidTrigger(format!("invalid trigger '{}': {e}", def.trigger)))?;
    // Additional validation for cron expressions and zero intervals
    match spec {
        crate::model::TriggerSpec::Cron { ref expression } => {
            scheduler_rs::trigger::CronTrigger::new(expression)
                .map_err(|e| PolicyError::InvalidTrigger(format!("invalid cron: {e}")))?;
        }
        crate::model::TriggerSpec::Interval { secs: 0 } => {
            return Err(PolicyError::InvalidTrigger("interval must be > 0 seconds".into()));
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct RawPolicyRow {
    id: u64,
    name: String,
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
