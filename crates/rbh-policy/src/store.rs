//! `PolicyStore` — CRUD for action policy rows in `rbh_entries.policies`.

use chrono::{DateTime, Utc};
use sqlx::{MySql, MySqlPool, Transaction, types::Json};

use crate::PolicyError;
use crate::model::{PolicyDef, PolicyRow};
use crate::rev8::{self, PreviewResult, resources::ResourceBinding};
use rbh_entry_store::EntryStore;

mod rev8_resources;
use crate::trigger_parser::parse_trigger;
pub use rev8_resources::ResourceRevision;

/// MariaDB-backed action policy store.
#[derive(Debug, Clone)]
pub struct PolicyStore {
    pool: MySqlPool,
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Rev8PolicyRow {
    pub id: u64,
    pub revision: u64,
    pub definition: Json<serde_json::Value>,
    pub resource_bindings: Option<Json<Vec<ResourceBinding>>>,
    pub modified_by: Option<String>,
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

    /// Bind rows upgraded from the pre-filesystem policy schema to the
    /// deployment's uniquely selected Lustre filesystem. This is idempotent.
    pub async fn bind_legacy_lustre_filesystem(
        &self, filesystem: &rbh_entry_store::FileSystemId,
    ) -> Result<u64, PolicyError> {
        let result = sqlx::query(
            "UPDATE policies SET definition = JSON_SET(definition, '$.filesystem', ?) \
             WHERE revision IS NULL AND JSON_UNQUOTE(JSON_EXTRACT(definition, '$.filesystem')) = '__legacy_lustre__'",
        )
        .bind(filesystem.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
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
            .await
            .map_err(|error| policy_write_error(error, &def.name))?;
        Ok(result.last_insert_id())
    }

    #[tracing::instrument(skip(self))]
    pub async fn get(&self, id: u64) -> Result<PolicyRow, PolicyError> {
        let row = sqlx::query_as::<_, RawPolicyRow>(
            "SELECT id, name, definition, enabled, created_at, updated_at FROM policies WHERE id = ? AND revision IS NULL",
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
            "SELECT id, name, definition, enabled, created_at, updated_at FROM policies WHERE name = ? AND revision IS NULL",
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
            "SELECT id, name, definition, enabled, created_at, updated_at FROM policies WHERE revision IS NULL ORDER BY id",
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
        let result = sqlx::query(
            "UPDATE policies SET name = ?, kind = ?, definition = ?, enabled = ? WHERE id = ? AND revision IS NULL",
        )
        .bind(&def.name)
        .bind(kind)
        .bind(&json)
        .bind(def.enabled)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|error| policy_write_error(error, &def.name))?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn delete(&self, id: u64) -> Result<(), PolicyError> {
        let result = sqlx::query("DELETE FROM policies WHERE id = ? AND revision IS NULL")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, entries, document))]
    pub async fn save_rev8(
        &self, entries: &EntryStore, id: Option<u64>, document: &serde_json::Value, actor: &str,
    ) -> Result<u64, PolicyError> {
        let input = rev8::parse(document)?;
        rev8::validate_filesystem(entries, input.filesystem()).await?;
        let mut tx = self.lock_configuration().await?;
        let (policy, bindings) = Self::bind_policy(&mut tx, input).await?;
        let id = if let Some(id) = id {
            let result = sqlx::query(
                "UPDATE policies SET name = ?, definition = ?, revision = revision + 1 WHERE id = ? AND revision IS NOT NULL",
            ).bind(&policy.name).bind(Json(document)).bind(id).execute(&mut *tx).await
                .map_err(|error| policy_write_error(error, &policy.name))?;
            if result.rows_affected() == 0 {
                return Err(PolicyError::NotFound(id));
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO policies (name, kind, definition, enabled, revision) VALUES (?, 'Policy', ?, FALSE, 1)",
            )
            .bind(&policy.name)
            .bind(Json(document))
            .execute(&mut *tx)
            .await
            .map_err(|error| policy_write_error(error, &policy.name))?
            .last_insert_id()
        };
        record_rev8_revision(&mut tx, id, bindings, actor).await?;
        tx.commit().await?;
        Ok(id)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_rev8(&self, id: u64) -> Result<Option<Rev8PolicyRow>, PolicyError> {
        Ok(sqlx::query_as(
            "SELECT r.policy_id AS id, r.revision, r.definition, r.resource_bindings, r.modified_by
             FROM policies p JOIN rev8_policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision WHERE p.id = ?",
        ).bind(id).fetch_optional(&self.pool).await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_rev8(&self) -> Result<Vec<Rev8PolicyRow>, PolicyError> {
        Ok(sqlx::query_as(
            "SELECT r.policy_id AS id, r.revision, r.definition, r.resource_bindings, r.modified_by
             FROM policies p JOIN rev8_policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision ORDER BY p.id",
        ).fetch_all(&self.pool).await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_rev8_revision(&self, id: u64, revision: u64) -> Result<Rev8PolicyRow, PolicyError> {
        sqlx::query_as(
            "SELECT policy_id AS id, revision, definition, resource_bindings, modified_by
             FROM rev8_policy_revisions WHERE policy_id = ? AND revision = ?",
        )
        .bind(id)
        .bind(revision)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(PolicyError::NotFound(id))
    }

    #[tracing::instrument(skip(self, entries))]
    pub async fn preview_rev8(&self, entries: &EntryStore, id: u64) -> Result<PreviewResult, PolicyError> {
        let row = self.get_rev8(id).await?.ok_or(PolicyError::NotFound(id))?;
        let bindings = row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default();
        let policy = self.compile_revision(&row.definition, &bindings).await?;
        policy.preview(entries, row.revision, bindings).await
    }
}

async fn record_rev8_revision(
    tx: &mut Transaction<'_, MySql>, id: u64, bindings: Vec<ResourceBinding>, actor: &str,
) -> Result<(), PolicyError> {
    sqlx::query(
        "INSERT INTO rev8_policy_revisions (policy_id, revision, definition, resource_bindings, modified_by)
         SELECT id, revision, definition, ?, ? FROM policies WHERE id = ?",
    )
    .bind(Json(bindings))
    .bind(actor)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn policy_write_error(error: sqlx::Error, name: &str) -> PolicyError {
    match &error {
        sqlx::Error::Database(db) if db.is_unique_violation() => PolicyError::NameConflict(name.to_string()),
        _ => PolicyError::Db(error),
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
