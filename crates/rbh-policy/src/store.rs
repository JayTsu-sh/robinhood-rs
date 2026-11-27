//! `PolicyStore` — CRUD for action policy rows in `rbh_entries.policies`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use chrono::Utc;
use sqlx::{MySql, MySqlPool, Transaction, types::Json};

use crate::PolicyError;
use crate::rev8::{
    self, CompiledPolicy, PreviewResult,
    heat::{HeatProfile, ReadHeatConfig},
    resources::ResourceBinding,
};
use rbh_entry_store::{EntryStore, FileSystemConfig, FileSystemId};

mod rev8_execution;
mod rev8_heat;
mod rev8_resources;
pub use rev8_execution::{Rev8Operation, Rev8Run};
pub use rev8_heat::HeatSampler;
pub use rev8_resources::ResourceRevision;

/// MariaDB-backed action policy store.
#[derive(Clone)]
pub struct PolicyStore {
    pool: MySqlPool,
    runtime: Option<rev8::execution::Runtime>,
    read_heat: Arc<BTreeMap<FileSystemId, HeatProfile>>,
    configured_filesystems: Option<Arc<BTreeSet<FileSystemId>>>,
    pool_recoveries: Arc<tokio::sync::Mutex<BTreeMap<u64, rev8_execution::PoolRecovery>>>,
    native_recoveries: Arc<tokio::sync::Mutex<BTreeMap<u64, rev8_execution::NativeRecovery>>>,
    pool_preflights: rev8_execution::PoolPreflights,
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
        Self {
            pool,
            runtime: None,
            read_heat: Arc::default(),
            configured_filesystems: None,
            pool_recoveries: Arc::default(),
            native_recoveries: Arc::default(),
            pool_preflights: rev8_execution::PoolPreflights::default(),
        }
    }

    pub fn with_rev8_runtime(mut self, runtime: rev8::execution::Runtime) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Restrict new policy work to the running daemon's deployment registry.
    pub fn with_configured_filesystems(mut self, filesystems: impl IntoIterator<Item = FileSystemId>) -> Self {
        self.configured_filesystems = Some(Arc::new(filesystems.into_iter().collect()));
        self
    }

    fn ensure_configured(&self, filesystem: &FileSystemId) -> Result<(), PolicyError> {
        if self
            .configured_filesystems
            .as_ref()
            .is_some_and(|configured| !configured.contains(filesystem))
        {
            return Err(rev8::invalid(format!(
                "filesystem {filesystem} is not configured in this deployment"
            )));
        }
        Ok(())
    }

    pub fn with_read_heat(
        mut self, filesystem: FileSystemConfig, config: ReadHeatConfig, executor_mounts: Arc<[PathBuf]>,
    ) -> Result<Self, PolicyError> {
        let id = filesystem.id.clone();
        let profile = HeatProfile::new(filesystem, config, executor_mounts)?;
        Arc::make_mut(&mut self.read_heat).insert(id, profile);
        Ok(self)
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

    #[tracing::instrument(skip(self, entries, document))]
    pub async fn save_rev8(
        &self, entries: &EntryStore, id: Option<u64>, document: &serde_json::Value, actor: &str,
    ) -> Result<u64, PolicyError> {
        let input = rev8::parse(document)?;
        self.ensure_configured(input.filesystem())?;
        let config = rev8::validate_filesystem(entries, input.filesystem()).await?;
        let activation = if input.enforced_and_enabled() {
            if !self.synchronous_permit_store().await? {
                return Err(rev8::invalid(
                    "enforced activation requires MariaDB innodb_flush_log_at_trx_commit=1",
                ));
            }
            let runtime = self.runtime.as_ref().ok_or_else(|| {
                PolicyError::InvalidPolicy("enforced activation requires a daemon HSM workload identity".into())
            })?;
            runtime.hsm.filesystem_name(&config.mount_path).await.map_err(|error| {
                PolicyError::InvalidPolicy(format!(
                    "enforced activation requires an accessible Lustre mount: {error}"
                ))
            })?;
            Some(runtime.principal.clone())
        } else {
            None
        };
        let next_run = input.schedule_after(Utc::now())?;
        let mut tx = self.lock_configuration().await?;
        let (policy, bindings) = Self::bind_policy(&mut tx, input).await?;
        policy.validate_capabilities(&config)?;
        if let Some(migration) = policy.pool_migration() {
            migration.validate_pools(&config.mount_path).await?;
        }
        if let Some(principal) = activation {
            if policy.uses_read_heat && !self.read_heat.contains_key(&policy.filesystem) {
                return Err(rev8::invalid(
                    "enforced read heat conditions require a configured Lustre read_heat profile",
                ));
            }
            if policy.actions.is_empty() {
                return Err(PolicyError::InvalidPolicy(
                    "enforced activation requires an execute rule".into(),
                ));
            }
            Self::compile_admission(&mut tx, &policy, &bindings, principal).await?;
        }
        let next_run = next_run.filter(|_| policy.enabled);
        let heat_filesystem =
            (policy.uses_read_heat && policy.pool_migration().is_some()).then_some(policy.filesystem.as_str());
        let id = if let Some(id) = id {
            let result = sqlx::query(
                "UPDATE policies SET name = ?, definition = ?, revision = revision + 1, enabled = ?, rev8_next_run = ?, rev8_heat_filesystem = ?
                 WHERE id = ? AND revision IS NOT NULL AND name IS NOT NULL",
            )
            .bind(&policy.name)
            .bind(Json(document))
            .bind(policy.enabled)
            .bind(next_run)
            .bind(heat_filesystem)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|error| policy_write_error(error, &policy.name))?;
            if result.rows_affected() == 0 {
                return Err(PolicyError::NotFound(id));
            }
            id
        } else {
            sqlx::query(
                "INSERT INTO policies (name, kind, definition, enabled, revision, rev8_next_run, rev8_heat_filesystem) VALUES (?, 'Policy', ?, ?, 1, ?, ?)",
            )
            .bind(&policy.name)
            .bind(Json(document))
            .bind(policy.enabled)
            .bind(next_run)
            .bind(heat_filesystem)
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
             FROM policies p JOIN rev8_policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision WHERE p.id = ? AND p.name IS NOT NULL",
        ).bind(id).fetch_optional(&self.pool).await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_rev8(&self) -> Result<Vec<Rev8PolicyRow>, PolicyError> {
        Ok(sqlx::query_as(
            "SELECT r.policy_id AS id, r.revision, r.definition, r.resource_bindings, r.modified_by
             FROM policies p JOIN rev8_policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision WHERE p.name IS NOT NULL ORDER BY p.id",
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

    #[tracing::instrument(skip(self))]
    pub async fn delete_rev8(&self, id: u64) -> Result<(), PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let result = sqlx::query(
            "UPDATE policies SET name = NULL, enabled = FALSE, rev8_next_run = NULL
             WHERE id = ? AND revision IS NOT NULL AND name IS NOT NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(PolicyError::NotFound(id));
        }
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(skip(self, entries))]
    pub async fn preview_rev8(&self, entries: &EntryStore, id: u64) -> Result<PreviewResult, PolicyError> {
        self.preview_rev8_with_heat(entries, id, None).await
    }

    #[tracing::instrument(skip(self, entries))]
    pub async fn preview_rev8_with_heat(
        &self, entries: &EntryStore, id: u64, fid: Option<lustre_api::LuFid>,
    ) -> Result<PreviewResult, PolicyError> {
        let row = self.get_rev8(id).await?.ok_or(PolicyError::NotFound(id))?;
        let bindings = row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default();
        let policy = self.compile_revision(rev8::parse(&row.definition)?, &bindings).await?;
        self.preview_compiled(entries, &policy, row.revision, bindings, fid)
            .await
    }

    async fn preview_compiled(
        &self, entries: &EntryStore, policy: &CompiledPolicy, revision: u64, bindings: Vec<ResourceBinding>,
        fid: Option<lustre_api::LuFid>,
    ) -> Result<PreviewResult, PolicyError> {
        if policy.uses_read_heat {
            rev8::heat::preview(
                entries,
                policy,
                self.read_heat.get(&policy.filesystem),
                revision,
                bindings,
                fid,
            )
            .await
        } else {
            policy.preview(entries, revision, bindings).await
        }
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
