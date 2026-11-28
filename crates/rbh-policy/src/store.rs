//! `PolicyStore` — CRUD for action policy rows in `rbh_entries.policies`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, RwLock},
    time::Duration,
};

use chrono::Utc;
use sqlx::{MySql, MySqlPool, Transaction, types::Json};

use crate::PolicyError;
use crate::policy::{
    self, CompiledPolicy, PreviewResult,
    heat::{HeatProfile, ReadHeatConfig},
    resources::ResourceBinding,
};
use rbh_entry_store::{EntryStore, FileSystemConfig, FileSystemId};

mod policy_execution;
mod policy_resources;
mod read_heat;
pub use policy_execution::history::{
    HistoryOperation, HistoryPage, LatestRun, OperationFilter, OperationResults, OperationSearch, OperationState,
    OperationSummary, RunActivationMode, RunHistory, RunReportSnapshot, RunSearch, RunState,
};
pub use policy_execution::{PolicyOperation, PolicyRun, RunControl};
pub use policy_resources::ResourceRevision;
pub use read_heat::HeatSampler;

type PoolJobHandle = tokio::task::JoinHandle<Result<(), PolicyError>>;

/// MariaDB-backed action policy store.
#[derive(Clone)]
pub struct PolicyStore {
    pool: MySqlPool,
    runtime: Option<policy::execution::Runtime>,
    read_heat: Arc<BTreeMap<FileSystemId, HeatProfile>>,
    configured_filesystems: Option<Arc<RwLock<BTreeSet<FileSystemId>>>>,
    console_mount_sources: Arc<RwLock<BTreeMap<FileSystemId, String>>>,
    pool_recoveries: Arc<tokio::sync::Mutex<BTreeMap<u64, policy_execution::PoolRecovery>>>,
    pool_jobs: Arc<tokio::sync::Mutex<BTreeMap<u64, PoolJobHandle>>>,
    native_recoveries: Arc<tokio::sync::Mutex<BTreeMap<u64, policy_execution::NativeRecovery>>>,
    native_submissions: Arc<tokio::sync::Mutex<BTreeMap<u64, policy_execution::NativeSubmissionJob>>>,
    pool_preflights: policy_execution::Preflights<policy_execution::PoolPreflight>,
    native_preflights: policy_execution::Preflights<policy_execution::NativePreflight>,
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRevision {
    pub id: u64,
    pub revision: u64,
    pub id_exact: String,
    pub revision_exact: String,
    pub definition: Json<serde_json::Value>,
    pub resource_bindings: Option<Json<Vec<ResourceBinding>>>,
    pub modified_by: Option<String>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_run: Option<LatestRun>,
    #[sqlx(skip)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy)]
pub enum PolicyWrite {
    Create,
    Update { id: u64, expected_revision: Option<u64> },
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPolicy {
    pub id: u64,
    pub revision: u64,
    pub id_exact: String,
    pub revision_exact: String,
}

impl SavedPolicy {
    fn new(id: u64, revision: u64) -> Self {
        Self {
            id,
            revision,
            id_exact: id.to_string(),
            revision_exact: revision.to_string(),
        }
    }
}

const POLICY_REVISION_FIELDS: &str = "r.policy_id AS id, r.revision, r.definition, r.resource_bindings, r.modified_by,
    CAST(r.policy_id AS CHAR) AS id_exact, CAST(r.revision AS CHAR) AS revision_exact";

const POLICY_HEAD_REVISION: &str = "SELECT revision FROM policies WHERE id = ? AND name IS NOT NULL";

fn check_policy_revision(id: u64, expected: Option<u64>, current: Option<u64>) -> Result<u64, PolicyError> {
    let current = current.ok_or(PolicyError::NotFound(id))?;
    if let Some(expected) = expected
        && expected != current
    {
        return Err(PolicyError::RevisionConflict { id, expected, current });
    }
    Ok(current)
}

impl PolicyStore {
    pub fn new(pool: MySqlPool) -> Self {
        Self {
            pool,
            runtime: None,
            read_heat: Arc::default(),
            configured_filesystems: None,
            console_mount_sources: Arc::default(),
            pool_recoveries: Arc::default(),
            pool_jobs: Arc::default(),
            native_recoveries: Arc::default(),
            native_submissions: Arc::default(),
            pool_preflights: policy_execution::Preflights::default(),
            native_preflights: policy_execution::Preflights::default(),
        }
    }

    pub fn with_execution_runtime(mut self, runtime: policy::execution::Runtime) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// Restrict new policy work to the running daemon's deployment registry.
    pub fn with_configured_filesystems(mut self, filesystems: impl IntoIterator<Item = FileSystemId>) -> Self {
        self.configured_filesystems = Some(Arc::new(RwLock::new(filesystems.into_iter().collect())));
        self
    }

    pub fn set_filesystem_configured(&self, filesystem: &FileSystemId, enabled: bool) {
        if let Some(configured) = &self.configured_filesystems {
            let mut configured = configured.write().expect("configured filesystem lock");
            if enabled {
                configured.insert(filesystem.clone());
            } else {
                configured.remove(filesystem);
            }
        }
    }

    pub fn set_console_mount_source(&self, filesystem: &FileSystemId, source: String) {
        self.console_mount_sources
            .write()
            .expect("console mount sources lock")
            .insert(filesystem.clone(), source);
    }

    /// Expected mount source for a registration managed by the console.
    pub fn console_mount_source(&self, filesystem: &FileSystemId) -> Option<String> {
        self.console_mount_sources
            .read()
            .expect("console mount sources lock")
            .get(filesystem)
            .cloned()
    }

    async fn console_mount_matches(&self, config: &FileSystemConfig) -> bool {
        let Some(source) = self.console_mount_source(&config.id) else {
            return true;
        };
        let path = config.mount_path.clone();
        tokio::time::timeout(
            Duration::from_secs(5),
            policy::execution::lustre_call(move || {
                lustre_api::pools::PoolMount::open(&path).is_ok_and(|mount| mount.source() == source)
            }),
        )
        .await
        .is_ok_and(|result| result.unwrap_or(false))
    }

    fn ensure_configured(&self, filesystem: &FileSystemId) -> Result<(), PolicyError> {
        if self.configured_filesystems.as_ref().is_some_and(|configured| {
            !configured
                .read()
                .expect("configured filesystem lock")
                .contains(filesystem)
        }) {
            return Err(policy::invalid(format!(
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

    #[tracing::instrument(skip(self, entries, document))]
    pub async fn save_policy(
        &self, entries: &EntryStore, write: PolicyWrite, document: &serde_json::Value, actor: &str,
    ) -> Result<SavedPolicy, PolicyError> {
        let input = policy::parse(document)?;
        // Report stale edits before probing resources; check again under the write lock.
        if let PolicyWrite::Update {
            id,
            expected_revision: Some(expected),
        } = write
        {
            let current = sqlx::query_scalar(POLICY_HEAD_REVISION)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
            check_policy_revision(id, Some(expected), current)?;
        }
        if let PolicyWrite::Update { id, expected_revision } = write
            && document["spec"]["enabled"] == false
            && let Some(saved) = self.deactivate_policy(id, expected_revision, document, actor).await?
        {
            return Ok(saved);
        }
        self.ensure_configured(input.filesystem())?;
        let config = policy::validate_filesystem(entries, input.filesystem()).await?;
        let activation = if input.enforced_and_enabled() {
            if !self.synchronous_permit_store().await? {
                return Err(policy::invalid(
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
            if input.has_directory_scope() {
                let mount = config.mount_path.clone();
                let probe =
                    policy::execution::lustre_call(move || lustre_api::LustreApi.verify_migration_namespace(&mount));
                tokio::time::timeout(Duration::from_secs(5), probe)
                    .await
                    .map_err(|_| policy::invalid("spec.target.directoryScope live FID-to-path check timed out"))?
                    .map_err(|error| PolicyError::Store(format!("live namespace probe: {error}")))?
                    .map_err(|error| policy::invalid(format!(
                        "spec.target.directoryScope requires live FID-to-path access on the executor mount. Check Lustre user_fid2path or the daemon's CAP_DAC_READ_SEARCH. {error:?}"
                    )))?;
            }
            Some(runtime.principal.clone())
        } else {
            None
        };
        let next_run = input.first_run(Utc::now())?;
        let mut tx = self.lock_configuration().await?;
        let update = match write {
            PolicyWrite::Create => None,
            PolicyWrite::Update { id, expected_revision } => {
                // Revision writers already hold the configuration lock. Leave the
                // policy row available to workers until the update is ready.
                let current = sqlx::query_scalar(POLICY_HEAD_REVISION)
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?;
                Some((id, check_policy_revision(id, expected_revision, current)?))
            }
        };
        // Removal and unmount take this lock and can complete during the probes above.
        self.ensure_configured(input.filesystem())?;
        if !self.console_mount_matches(&config).await {
            return Err(policy::invalid(
                "registered Lustre mount source has changed or is unavailable",
            ));
        }
        let (policy, bindings) = Self::bind_policy(&mut tx, input).await?;
        policy.validate_capabilities(&config)?;
        if let Some(migration) = policy.pool_migration() {
            migration.validate_pools(&config.mount_path).await?;
        }
        if let Some(principal) = activation {
            if policy.uses_read_heat && !self.read_heat.contains_key(&policy.filesystem) {
                return Err(policy::invalid(
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
        let (id, revision) = if let Some((id, current)) = update {
            sqlx::query(
                "UPDATE policies SET name = ?, definition = ?, revision = revision + 1, enabled = ?, next_run = ?, heat_filesystem = ?
                 WHERE id = ?",
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
            (id, current + 1)
        } else {
            let id = sqlx::query(
                "INSERT INTO policies (name, definition, enabled, revision, next_run, heat_filesystem) VALUES (?, ?, ?, 1, ?, ?)",
            )
            .bind(&policy.name)
            .bind(Json(document))
            .bind(policy.enabled)
            .bind(next_run)
            .bind(heat_filesystem)
            .execute(&mut *tx)
            .await
            .map_err(|error| policy_write_error(error, &policy.name))?
            .last_insert_id();
            (id, 1)
        };
        record_policy_revision(&mut tx, id, bindings, actor).await?;
        tx.commit().await?;
        Ok(SavedPolicy::new(id, revision))
    }

    // Stopping dispatch must work even if the resources used by the current
    // policy have become unavailable. Edits still go through full validation.
    async fn deactivate_policy(
        &self, id: u64, expected_revision: Option<u64>, document: &serde_json::Value, actor: &str,
    ) -> Result<Option<SavedPolicy>, PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let row: PolicyRevision = sqlx::query_as(&format!(
            "SELECT {POLICY_REVISION_FIELDS}
             FROM policies p JOIN policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision
             WHERE p.id = ? AND p.name IS NOT NULL"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PolicyError::NotFound(id))?;
        let revision = check_policy_revision(id, expected_revision, Some(row.revision))?;
        let mut stopped = row.definition.0;
        if stopped["spec"]["enabled"] == false {
            return Ok(None);
        }
        stopped["spec"]["enabled"] = false.into();
        if &stopped != document {
            return Ok(None);
        }
        sqlx::query(
            "UPDATE policies SET definition = ?, enabled = FALSE, revision = revision + 1, next_run = NULL
             WHERE id = ?",
        )
        .bind(Json(document))
        .bind(id)
        .execute(&mut *tx)
        .await?;
        record_policy_revision(
            &mut tx,
            id,
            row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default(),
            actor,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(SavedPolicy::new(id, revision + 1)))
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_policy(&self, id: u64) -> Result<Option<PolicyRevision>, PolicyError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {POLICY_REVISION_FIELDS}
             FROM policies p JOIN policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision WHERE p.id = ? AND p.name IS NOT NULL"
        )).bind(id).fetch_optional(&self.pool).await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_policies(&self) -> Result<Vec<PolicyRevision>, PolicyError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {POLICY_REVISION_FIELDS}
             FROM policies p JOIN policy_revisions r ON r.policy_id = p.id AND r.revision = p.revision WHERE p.name IS NOT NULL ORDER BY p.id"
        )).fetch_all(&self.pool).await?)
    }

    /// List current revisions whose target belongs to this deployment's registry.
    #[tracing::instrument(skip(self))]
    pub async fn list_configured_policies(&self) -> Result<Vec<PolicyRevision>, PolicyError> {
        let mut rows = self.list_policies().await?;
        self.retain_configured_policies(&mut rows);
        Ok(rows)
    }

    fn retain_configured_policies(&self, rows: &mut Vec<PolicyRevision>) {
        if let Some(configured) = &self.configured_filesystems {
            let configured = configured.read().expect("configured filesystem lock");
            rows.retain(|row| {
                row.definition["spec"]["target"]["filesystem"]
                    .as_str()
                    .and_then(|id| FileSystemId::new(id).ok())
                    .is_some_and(|id| configured.contains(&id))
            });
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_policy_revision(&self, id: u64, revision: u64) -> Result<PolicyRevision, PolicyError> {
        Self::read_policy_revision(id, revision, &self.pool).await
    }

    async fn read_policy_revision(
        id: u64, revision: u64, executor: impl sqlx::MySqlExecutor<'_>,
    ) -> Result<PolicyRevision, PolicyError> {
        sqlx::query_as(&format!(
            "SELECT {POLICY_REVISION_FIELDS}
             FROM policy_revisions r WHERE policy_id = ? AND revision = ?"
        ))
        .bind(id)
        .bind(revision)
        .fetch_optional(executor)
        .await?
        .ok_or(PolicyError::NotFound(id))
    }

    #[tracing::instrument(skip(self))]
    pub async fn delete_policy(&self, id: u64) -> Result<(), PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let result = sqlx::query(
            "UPDATE policies SET name = NULL, enabled = FALSE, next_run = NULL
             WHERE id = ? AND name IS NOT NULL",
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
    pub async fn preview_policy(&self, entries: &EntryStore, id: u64) -> Result<PreviewResult, PolicyError> {
        self.preview_policy_with_heat(entries, id, None).await
    }

    #[tracing::instrument(skip(self, entries))]
    pub async fn preview_policy_with_heat(
        &self, entries: &EntryStore, id: u64, fid: Option<lustre_api::LuFid>,
    ) -> Result<PreviewResult, PolicyError> {
        let row = self.get_policy(id).await?.ok_or(PolicyError::NotFound(id))?;
        let bindings = row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default();
        let policy = self
            .compile_revision(policy::parse(&row.definition)?, &bindings)
            .await?;
        policy
            .preview_with_heat(
                entries,
                row.revision,
                bindings,
                self.read_heat.get(&policy.filesystem),
                fid,
                true,
            )
            .await
    }

    /// Compile an unsaved document against a read-only view of the current policy resources.
    #[tracing::instrument(skip_all, fields(filesystem))]
    pub async fn preview_policy_document(
        &self, entries: &EntryStore, document: &serde_json::Value,
    ) -> Result<PreviewResult<()>, PolicyError> {
        let input = policy::parse(document)?;
        tracing::Span::current().record("filesystem", tracing::field::display(input.filesystem()));
        self.ensure_configured(input.filesystem())?;
        let mut tx = self.pool.begin_with("START TRANSACTION READ ONLY").await?;
        let (policy, bindings) = Self::bind_policy(&mut tx, input).await?;
        tx.commit().await?;
        let migration = policy
            .pool_migration()
            .ok_or_else(|| policy::invalid("draft tiering preview requires lustre.pool_migrate"))?;
        let config = policy::validate_filesystem(entries, &policy.filesystem).await?;
        if !self.console_mount_matches(&config).await {
            return Err(policy::invalid(
                "registered Lustre mount source has changed or is unavailable",
            ));
        }
        if !policy.has_directory_scope() {
            policy.validate_capabilities(&config)?;
            migration.validate_pools(&config.mount_path).await?;
        }
        policy
            .preview_with_heat(
                entries,
                (),
                bindings,
                self.read_heat.get(&policy.filesystem),
                None,
                true,
            )
            .await
    }

    async fn preview_compiled<Revision>(
        &self, entries: &EntryStore, policy: &CompiledPolicy, revision: Revision, bindings: Vec<ResourceBinding>,
        fid: Option<lustre_api::LuFid>,
    ) -> Result<PreviewResult<Revision>, PolicyError> {
        policy
            .preview_with_heat(
                entries,
                revision,
                bindings,
                self.read_heat.get(&policy.filesystem),
                fid,
                false,
            )
            .await
    }
}

async fn record_policy_revision(
    tx: &mut Transaction<'_, MySql>, id: u64, bindings: Vec<ResourceBinding>, actor: &str,
) -> Result<(), PolicyError> {
    sqlx::query(
        "INSERT INTO policy_revisions (policy_id, revision, definition, resource_bindings, modified_by)
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
