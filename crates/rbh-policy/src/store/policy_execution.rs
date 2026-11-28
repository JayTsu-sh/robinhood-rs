pub(super) mod history;
mod migration;
mod native_admission;
mod native_recovery;
mod preflight;
pub(super) use migration::{PoolPreflight, PoolRecovery};
pub(super) use native_admission::{NativePreflight, NativeSubmissionJob};
pub(super) use native_recovery::NativeRecovery;
pub(super) use preflight::Preflights;

use std::{
    collections::BTreeMap,
    ops::ControlFlow,
    sync::{Arc, OnceLock},
    time::Instant,
};

use chrono::{DateTime, Utc};
use lustre_api::{LuFid, migration::MigrationPlan};
use rbh_entry_store::{EntryStore, FileSystemConfig, fid_codec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{MySql, MySqlExecutor, QueryBuilder, Transaction, types::Json};

use super::PolicyStore;
use crate::{
    PolicyError,
    policy::{
        self, ActionKind, BudgetLimits, CompiledPolicy, DB_EPOCH_MICROS, Invocation, ScopeSummary,
        execution::{Admission, Generation, Profile},
        heat::{self, HeatProfile},
        invalid, push_expression,
        resources::safety_matches,
    },
};

const BATCH_SIZE: usize = 64;
const RUN_FIELDS: &str =
    "id, policy_id, revision, state, reason, candidate_count, estimated_bytes, requested_by, approved_by, created_at, started_at,
     heat_deferred_by_reason, directory_scope, paused_millis,
     CASE WHEN state IN ('previewed','finished','deferred','cancelled') THEN updated_at END AS finished_at";
const OPERATION_FIELDS: &str =
    "o.id, o.run_id, r.policy_id, r.revision, o.filesystem, o.fid, o.action, o.archive_id, o.state,
     o.reason, o.attempted, o.generation, o.principal, o.size, o.accepted_at, o.completion_evidence, o.migration_plan,
     o.checked_at, o.paths, o.attempt_count";

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRun {
    pub id: u64,
    pub policy_id: u64,
    pub revision: u64,
    pub state: String,
    pub reason: Option<String>,
    pub candidate_count: Option<u64>,
    pub estimated_bytes: Option<u64>,
    pub requested_by: Option<String>,
    pub approved_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(skip)]
    paused_millis: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heat_deferred_by_reason: Option<Json<BTreeMap<String, u64>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory_scope: Option<Json<ScopeSummary>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunControl {
    Pause,
    Resume,
    Cancel,
    Approve,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct PolicyOperation {
    pub id: u64,
    pub run_id: u64,
    pub policy_id: u64,
    pub revision: u64,
    pub filesystem: String,
    #[serde(serialize_with = "serialize_fid")]
    fid: Vec<u8>,
    pub action: String,
    pub archive_id: u32,
    pub state: String,
    pub reason: Option<String>,
    pub attempted: bool,
    pub attempt_count: Option<u32>,
    pub paths: Option<Json<Vec<String>>>,
    pub generation: Option<Json<Generation>>,
    pub principal: Option<Json<Value>>,
    pub size: u64,
    pub accepted_at: Option<DateTime<Utc>>,
    pub completion_evidence: Option<Json<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_plan: Option<Json<MigrationPlan>>,
    #[serde(skip)]
    checked_at: Option<DateTime<Utc>>,
}

fn serialize_fid<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    decode_fid(bytes)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

fn decode_fid(bytes: &[u8]) -> Result<LuFid, PolicyError> {
    fid_codec::decode(bytes).ok_or_else(|| PolicyError::Store("invalid persisted Lustre FID".into()))
}

#[derive(Clone, sqlx::FromRow)]
struct Candidate {
    fid: Vec<u8>,
    size: u64,
    mtime: i64,
    ctime: i64,
    atime: i64,
    branch: i64,
    sort_value: Option<i64>,
}

#[derive(Default, sqlx::FromRow)]
struct Usage {
    action: String,
    operations: u64,
    attempts: u64,
    bytes: u64,
}

impl Usage {
    fn add(&mut self, other: &Self) {
        self.operations = self.operations.saturating_add(other.operations);
        self.attempts = self.attempts.saturating_add(other.attempts);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }

    fn exceeds(&self, limits: BudgetLimits, size: u64, elapsed_ms: u64) -> bool {
        self.exhausted(limits, elapsed_ms) || limits.bytes.checked_sub(self.bytes).is_none_or(|left| size > left)
    }

    fn exhausted(&self, limits: BudgetLimits, elapsed_ms: u64) -> bool {
        self.operations >= limits.operations
            || self.attempts >= limits.attempts
            || limits.duration_ms.is_some_and(|limit| elapsed_ms >= limit)
    }
}

#[derive(sqlx::FromRow)]
struct Outstanding {
    filesystem: String,
    action: String,
    operations: i64,
}

enum AdmissionBlock {
    PolicyBudget,
    SafetyBudget,
    OutstandingLimit,
    ProfileLimit,
    MigrationBusy,
}

impl AdmissionBlock {
    fn reason(&self) -> &'static str {
        match self {
            Self::PolicyBudget => "policy_run_budget",
            Self::SafetyBudget => "system_safety_run_budget",
            Self::OutstandingLimit => "system_safety_outstanding_limit",
            Self::ProfileLimit => "profile_concurrency_limit",
            Self::MigrationBusy => "filesystem_migration_busy",
        }
    }

    fn is_capacity(&self) -> bool {
        matches!(self, Self::OutstandingLimit | Self::ProfileLimit | Self::MigrationBusy)
    }
}

struct Dispatch<'a> {
    run: &'a PolicyRun,
    policy: &'a Arc<CompiledPolicy>,
    admission: Arc<Admission>,
    config: &'a FileSystemConfig,
    heat: Option<&'a HeatProfile>,
    preflight_until: OnceLock<Instant>,
}

struct MigrationReservation<'a> {
    plan: &'a MigrationPlan,
    paths: &'a [String],
    attempts: u32,
    lease_until: DateTime<Utc>,
}

impl PolicyStore {
    pub(super) async fn synchronous_permit_store(&self) -> Result<bool, PolicyError> {
        Ok(
            sqlx::query_scalar::<_, u64>("SELECT CAST(@@GLOBAL.innodb_flush_log_at_trx_commit AS UNSIGNED)")
                .fetch_one(&self.pool)
                .await?
                == 1,
        )
    }

    #[tracing::instrument(skip(self))]
    pub async fn request_policy_run(
        &self, id: u64, actor: &str, expected_revision: Option<u64>,
    ) -> Result<u64, PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let row: Option<(u64, Json<Value>)> =
            sqlx::query_as("SELECT revision, definition FROM policies WHERE id = ? AND name IS NOT NULL FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let (revision, definition) = row.ok_or(PolicyError::NotFound(id))?;
        super::check_policy_revision(id, expected_revision, Some(revision))?;
        if definition["spec"]["enabled"] == false {
            return Err(invalid("the policy is disabled"));
        }
        if expected_revision.is_some()
            && let Some(existing) = sqlx::query_scalar(
                "SELECT r.id FROM policy_runs r WHERE r.policy_id = ? AND r.revision = ? AND (
                   r.state IN ('queued','running','paused','awaiting_approval') OR EXISTS (
                     SELECT 1 FROM policy_operations o WHERE o.run_id = r.id AND o.pending_migration_fs IS NOT NULL
                   )) ORDER BY r.state = 'running' DESC, r.id DESC LIMIT 1",
            )
            .bind(id)
            .bind(revision)
            .fetch_optional(&mut *tx)
            .await?
        {
            return Err(PolicyError::ActiveRun { id: existing });
        }
        if definition["spec"]["mode"] == "one_shot"
            && let Some(existing) = existing_one_shot_run(&mut tx, id, revision, &definition).await?
        {
            return if expected_revision.is_some() {
                Err(PolicyError::OneShotRun { id: existing })
            } else {
                Ok(existing)
            };
        }
        let run = sqlx::query("INSERT INTO policy_runs (policy_id, revision, requested_by) VALUES (?, ?, ?)")
            .bind(id)
            .bind(revision)
            .bind(actor)
            .execute(&mut *tx)
            .await?
            .last_insert_id();
        tx.commit().await?;
        Ok(run)
    }

    /// Serialize operator requests with dispatch, leaving submitted operations available for reconciliation.
    #[tracing::instrument(skip(self))]
    pub async fn control_run(&self, id: u64, control: RunControl, actor: &str) -> Result<(), PolicyError> {
        let mut tx = self.pool.begin().await?;
        Self::lock_run_worker(&mut tx).await?;
        let row: Option<(String, bool, bool)> = sqlx::query_as(
            "SELECT CAST(r.state AS CHAR), r.started_at IS NOT NULL,
                    COALESCE(p.enabled AND p.name IS NOT NULL AND p.revision = r.revision, FALSE)
             FROM policy_runs r JOIN policies p ON p.id = r.policy_id WHERE r.id = ?",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let (state, started, current) = row.ok_or(PolicyError::NotFound(id))?;
        let (next, reason) = match (control, state.as_str(), current) {
            (RunControl::Pause, "queued" | "running", true) => ("paused", "paused_by_operator"),
            (RunControl::Resume, "paused", true) => (if started { "running" } else { "queued" }, "resumed_by_operator"),
            (RunControl::Cancel, "queued" | "running" | "paused" | "awaiting_approval", _) => {
                ("cancelled", "cancelled_by_operator")
            }
            (RunControl::Approve, "awaiting_approval", true) => ("running", "approved_by_operator"),
            (RunControl::Pause, "paused", true) | (RunControl::Cancel, "cancelled", _) => return Ok(()),
            _ => return Err(invalid(format!("cannot {control:?} run in state {state}"))),
        };
        sqlx::query(
            "UPDATE policy_runs SET paused_millis = paused_millis +
             CASE WHEN state IN ('paused','awaiting_approval') AND started_at IS NOT NULL AND ? = 'running'
               THEN GREATEST(TIMESTAMPDIFF(MICROSECOND, updated_at, UTC_TIMESTAMP(6)) DIV 1000, 0) ELSE 0 END,
             approved_by = COALESCE(?, approved_by), state = ?, reason = ?, updated_at = UTC_TIMESTAMP(6) WHERE id = ?",
        )
        .bind(next)
        .bind(matches!(control, RunControl::Approve).then_some(actor))
        .bind(next)
        .bind(reason)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO policy_run_controls (run_id, action, actor, occurred_at) VALUES (?, ?, ?, UTC_TIMESTAMP(6))",
        )
        .bind(id)
        .bind(match control {
            RunControl::Pause => "pause",
            RunControl::Resume => "resume",
            RunControl::Cancel => "cancel",
            RunControl::Approve => "approve",
        })
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(
            run_id = id,
            actor,
            ?control,
            state = next,
            "operator changed policy run state"
        );
        Ok(())
    }

    async fn lock_run_worker(tx: &mut Transaction<'_, MySql>) -> Result<(), PolicyError> {
        sqlx::query_scalar::<_, u8>("SELECT id FROM policy_worker_lock WHERE id = 1 FOR UPDATE")
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(PolicyError::WorkerLockMissing)?;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_policy_runs(&self, id: u64, after: u64) -> Result<Vec<PolicyRun>, PolicyError> {
        let exists = sqlx::query_scalar::<_, u64>("SELECT id FROM policies WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            return Err(PolicyError::NotFound(id));
        }
        Ok(sqlx::query_as(&format!(
            "SELECT {RUN_FIELDS} FROM policy_runs WHERE policy_id = ? AND id > ? ORDER BY id LIMIT 100"
        ))
        .bind(id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_run_operations(
        &self, policy: u64, run: u64, after: u64,
    ) -> Result<Vec<PolicyOperation>, PolicyError> {
        let exists = sqlx::query_scalar::<_, u64>("SELECT id FROM policy_runs WHERE id = ? AND policy_id = ?")
            .bind(run)
            .bind(policy)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            return Err(PolicyError::NotFound(run));
        }
        Ok(sqlx::query_as(&format!(
            "SELECT {OPERATION_FIELDS} FROM policy_operations o JOIN policy_runs r ON r.id = o.run_id
            WHERE r.id = ? AND o.id > ? ORDER BY o.id LIMIT 100"
        ))
        .bind(run)
        .bind(after)
        .fetch_all(&self.pool)
        .await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn reset_policy_breaker(&self, id: u64, actor: &str) -> Result<(), PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let active =
            sqlx::query_scalar::<_, u64>("SELECT id FROM policies WHERE id = ? AND name IS NOT NULL FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if active.is_none() {
            return Err(PolicyError::NotFound(id));
        }
        sqlx::query(
            "INSERT INTO policy_breakers (policy_id, reset_by, reset_at)
             VALUES (?, ?, UTC_TIMESTAMP(6)) ON DUPLICATE KEY UPDATE
             reset_by = VALUES(reset_by), reset_at = VALUES(reset_at), opened = FALSE",
        )
        .bind(id)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Advance a bounded batch. The database lock serializes workers that share
    /// a catalog. The submission record uses a separate, committed transaction.
    #[tracing::instrument(skip_all, fields(worker = "policy"))]
    pub async fn process_policy_queue(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        let mut worker = self.pool.begin().await?;
        Self::lock_run_worker(&mut worker).await?;

        self.reconcile_operations(entries).await?;
        sqlx::query(
            "UPDATE policy_runs r JOIN policies p ON p.id = r.policy_id SET
             r.state = 'cancelled', r.reason = 'policy_revision_inactive', r.updated_at = UTC_TIMESTAMP(6)
             WHERE r.state IN ('paused','awaiting_approval') AND (p.name IS NULL OR NOT p.enabled OR p.revision <> r.revision)",
        )
        .execute(&self.pool)
        .await?;
        self.enqueue_due_policies().await?;
        let run: Option<PolicyRun> = sqlx::query_as(&format!(
            "SELECT {RUN_FIELDS} FROM policy_runs WHERE state IN ('queued', 'running') ORDER BY updated_at, id LIMIT 1"
        ))
        .fetch_optional(&self.pool)
        .await?;
        if let Some(run) = run {
            self.advance_run(entries, run, &mut worker).await?;
        }
        worker.commit().await?;
        Ok(())
    }

    async fn enqueue_due_policies(&self) -> Result<(), PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let due: Vec<(u64, u64, Json<Value>)> = sqlx::query_as(
            "SELECT p.id, p.revision, p.definition FROM policies p
             WHERE p.enabled AND p.next_run <= UTC_TIMESTAMP(6) AND NOT EXISTS (
                SELECT 1 FROM policy_runs r WHERE r.policy_id = p.id AND r.revision = p.revision
                AND (r.state IN ('queued','running','paused','awaiting_approval') OR EXISTS (
                    SELECT 1 FROM policy_operations o WHERE o.run_id = r.id AND o.pending_migration_fs IS NOT NULL
                )))
             ORDER BY p.next_run, p.id LIMIT 64 FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (id, revision, definition) in due {
            let policy = policy::parse(&definition)?;
            let next = policy.next_run(Utc::now())?;
            sqlx::query("UPDATE policies SET next_run = ? WHERE id = ?")
                .bind(next)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            if next.is_none()
                && existing_one_shot_run(&mut tx, id, revision, &definition)
                    .await?
                    .is_some()
            {
                continue;
            }
            sqlx::query("INSERT INTO policy_runs (policy_id, revision) VALUES (?, ?)")
                .bind(id)
                .bind(revision)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(run_id = run.id, policy_id = run.policy_id, revision = run.revision))]
    async fn advance_run(
        &self, entries: &EntryStore, mut run: PolicyRun, worker: &mut Transaction<'_, MySql>,
    ) -> Result<(), PolicyError> {
        let Some(row) = self.get_policy(run.policy_id).await? else {
            return self.finish_run(&run, "cancelled", Some("policy_deleted"), None).await;
        };
        if row.revision != run.revision || row.definition["spec"]["enabled"] == false {
            return self
                .finish_run(&run, "cancelled", Some("policy_revision_inactive"), None)
                .await;
        }
        let policy = policy::parse(&row.definition)?;
        if self.ensure_configured(policy.filesystem()).is_err() {
            return self
                .finish_run(&run, "deferred", Some("filesystem_not_configured"), None)
                .await;
        }
        let bindings = row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default();
        let policy = Arc::new(self.compile_revision(policy, &bindings).await?);
        if run.reason.as_deref() == Some("migration_reconciliation_pending") {
            return if self.has_pending_migrations(run.id).await? {
                self.finish_run(&run, "running", Some("migration_reconciliation_pending"), None)
                    .await
            } else {
                self.finish_run(&run, "finished", None, None).await
            };
        }
        if run.requested_by.is_none()
            && run.candidate_count.is_none()
            && let Some((high, _)) = policy.pool_watermarks
        {
            let config = policy::validate_filesystem(entries, &policy.filesystem).await?;
            let migration = policy.pool_migration().expect("validated pool usage trigger");
            let Some((total, used, _)) = policy::pool_capacity(&config.mount_path, &migration.source_pool).await else {
                return self
                    .finish_run(&run, "deferred", Some("source_pool_capacity_unavailable"), None)
                    .await;
            };
            if used * 100 < total * u128::from(high) {
                return self
                    .finish_run(&run, "deferred", Some("source_pool_below_high_watermark"), None)
                    .await;
            }
        }
        let preview = if run.candidate_count.is_none() {
            match self
                .preview_compiled(entries, &policy, row.revision, bindings.clone(), None)
                .await
            {
                Ok(preview) => Some(preview),
                Err(PolicyError::InvalidPolicy(reason)) => {
                    return self.finish_run(&run, "deferred", Some(&reason), None).await;
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let now = Utc::now();
        if let Some(preview) = preview {
            let mut tx = self.pool.begin().await?;
            let active = current_revision(&mut tx, &run).await?;
            let needs_approval =
                active && policy.enforced && policy.require_operator_approval && run.approved_by.is_none();
            let (state, reason) = if !active {
                ("cancelled", Some("policy_revision_inactive"))
            } else if needs_approval {
                ("awaiting_approval", Some("operator_approval_required"))
            } else if policy.enforced {
                ("running", None)
            } else {
                ("previewed", None)
            };
            sqlx::query(
                "UPDATE policy_runs SET state = ?, reason = ?, candidate_count = ?, estimated_bytes = ?, started_at = ?,
                 heat_deferred_by_reason = ?, directory_scope = ? WHERE id = ?",
            )
            .bind(state)
            .bind(reason)
            .bind(preview.candidate_count)
            .bind(preview.estimated_bytes)
            .bind(now)
            .bind(preview.read_heat.map(|heat| Json(heat.deferred_by_reason)))
            .bind(preview.directory_scope.map(Json))
            .bind(run.id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            if !active || !policy.enforced || needs_approval {
                return Ok(());
            }
        }
        run.started_at.get_or_insert(now);
        let Some(runtime) = self.runtime.as_ref() else {
            return self
                .finish_run(&run, "deferred", Some("workload_identity_unavailable"), None)
                .await;
        };
        let config = match policy::validate_filesystem(entries, &policy.filesystem).await {
            Ok(config) => config,
            Err(PolicyError::InvalidPolicy(reason)) => {
                return self.finish_run(&run, "deferred", Some(&reason), None).await;
            }
            Err(error) => return Err(error),
        };
        if !self.console_mount_matches(&config).await {
            return self
                .finish_run(&run, "deferred", Some("registered_mount_source_changed"), None)
                .await;
        }
        if let Err(PolicyError::InvalidPolicy(reason)) = policy.validate_capabilities(&config) {
            return self.finish_run(&run, "deferred", Some(&reason), None).await;
        }
        let mut tx = self.pool.begin().await?;
        let admission = Self::compile_admission(&mut tx, &policy, &bindings, runtime.principal.clone()).await;
        tx.commit().await?;
        let admission = match admission {
            Ok(admission) => admission,
            Err(PolicyError::InvalidPolicy(reason)) => {
                return self.finish_run(&run, "deferred", Some(&reason), None).await;
            }
            Err(error) => return Err(error),
        };
        if !self.synchronous_permit_store().await? {
            return self
                .finish_run(&run, "deferred", Some("permit_store_requires_synchronous_commit"), None)
                .await;
        }
        if !admission.window_open(Utc::now()) {
            return self
                .finish_run(&run, "deferred", Some("admission_window_closed"), None)
                .await;
        }
        let heat = if policy.uses_read_heat {
            let Some(profile) = self.read_heat.get(&policy.filesystem) else {
                return self
                    .finish_run(&run, "deferred", Some(heat::NOT_CONFIGURED), None)
                    .await;
            };
            Some(profile)
        } else {
            None
        };
        let dispatch = Dispatch {
            run: &run,
            policy: &policy,
            admission: Arc::new(admission),
            config: &config,
            heat,
            preflight_until: OnceLock::new(),
        };
        self.dispatch_batch(dispatch, worker).await
    }

    async fn finish_run(
        &self, run: &PolicyRun, state: &str, reason: Option<&str>, heat_deferrals: Option<Json<BTreeMap<String, u64>>>,
    ) -> Result<(), PolicyError> {
        let mut tx = self.pool.begin().await?;
        let (state, reason) = if state != "cancelled" && !current_revision(&mut tx, run).await? {
            ("cancelled", Some("policy_revision_inactive"))
        } else {
            (state, reason)
        };
        sqlx::query(
            "UPDATE policy_runs SET state = ?, reason = ?, updated_at = UTC_TIMESTAMP(6),
             heat_deferred_by_reason = COALESCE(?, heat_deferred_by_reason) WHERE id = ?",
        )
        .bind(state)
        .bind(reason.map(short_reason))
        .bind(heat_deferrals)
        .bind(run.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn dispatch_batch(
        &self, dispatch: Dispatch<'_>, worker: &mut Transaction<'_, MySql>,
    ) -> Result<(), PolicyError> {
        let run = dispatch.run;
        let policy = dispatch.policy;
        let (cursor, cursor_value): (Option<Vec<u8>>, Option<i64>) =
            sqlx::query_as("SELECT cursor_id, cursor_value FROM policy_runs WHERE id = ?")
                .bind(run.id)
                .fetch_one(&self.pool)
                .await?;
        let order = policy.candidate_order.sort_column();
        let start = [0; 16];
        let after = cursor.as_deref().unwrap_or(&start);
        let mut snapshot = if dispatch.heat.is_some() {
            Some(
                self.pool
                    .begin_with("START TRANSACTION WITH CONSISTENT SNAPSHOT")
                    .await?,
            )
        } else {
            None
        };
        let database_time_micros = if let Some(tx) = &mut snapshot {
            let sql = format!("SELECT CAST({DB_EPOCH_MICROS} AS SIGNED)");
            Some(sqlx::query_scalar::<_, i64>(&sql).fetch_one(&mut **tx).await?)
        } else {
            None
        };
        let now = Utc::now();
        let mut query = QueryBuilder::new("");
        policy.push_namespace_paths(&mut query, None);
        query.push("SELECT entries.object_id AS fid, size, mtime, ctime, atime, CAST((");
        push_expression(&mut query, &policy.decision, &policy.decision_params);
        query.push(") AS SIGNED) AS branch, ");
        if let Some((column, _)) = order {
            query.push(format!("CAST({column} AS SIGNED)"));
        } else {
            query.push("NULL");
        }
        query.push(" AS sort_value FROM scoped_entries AS entries ");
        if let Some(profile) = dispatch.heat {
            heat::push_scores(&mut query, profile, now, None);
        }
        query
            .push(" WHERE entries.filesystem_id = ")
            .push_bind(policy.filesystem.as_str())
            .push(" AND entries.object_kind = 0");
        if let Some((column, ascending)) = order {
            if let Some(value) = cursor_value {
                let after_key = if ascending { ">" } else { "<" };
                query
                    .push(format!(" AND ({column} {after_key} "))
                    .push_bind(value)
                    .push(format!(" OR ({column} = "))
                    .push_bind(value)
                    .push(" AND entries.object_id > ")
                    .push_bind(after)
                    .push("))");
            }
        } else {
            query.push(" AND entries.object_id > ").push_bind(after);
        }
        query.push(" AND (");
        policy.push_scope(&mut query, database_time_micros);
        query.push(") HAVING branch > 0 ORDER BY ");
        if let Some((column, ascending)) = order {
            let direction = if ascending { "ASC" } else { "DESC" };
            query.push(format!("{column} {direction}, "));
        }
        query.push(format!("entries.object_id LIMIT {BATCH_SIZE}"));
        let select = query.build_query_as::<Candidate>();
        let candidates = match snapshot.as_mut() {
            Some(tx) => select.fetch_all(&mut **tx).await?,
            None => select.fetch_all(&self.pool).await?,
        };
        let page_end = candidates.last().filter(|_| candidates.len() == BATCH_SIZE);
        let through = page_end.map(|row| row.fid.as_slice());
        let mut heat_deferrals = None;
        if let (Some(mut tx), Some(profile)) = (snapshot, dispatch.heat)
            && order.is_none()
        {
            // Use the same view and freshness clock as candidate selection.
            // Count only the range visited by this page before advancing it.
            let mut page =
                heat::dispatch_deferrals(&mut tx, policy, profile, now, database_time_micros, after, through).await?;
            tx.commit().await?;
            if let Some(Json(prior)) = run.heat_deferred_by_reason.as_ref().filter(|_| cursor.is_some()) {
                for (reason, count) in prior {
                    *page.entry(reason.clone()).or_default() += count;
                }
            }
            heat_deferrals = Some(Json(page));
        }
        let mut awaiting_predecessor = false;
        for candidate in &candidates {
            let recorded =
                sqlx::query_scalar::<_, u64>("SELECT id FROM policy_operations WHERE run_id = ? AND fid = ?")
                    .bind(run.id)
                    .bind(&candidate.fid)
                    .fetch_optional(&self.pool)
                    .await?;
            if recorded.is_some() {
                continue;
            }
            if run.requested_by.is_none()
                && let Some((_, low)) = policy.pool_watermarks
            {
                let migration = policy.pool_migration().expect("validated pool usage trigger");
                let Some((total, used, _)) =
                    policy::pool_capacity(&dispatch.config.mount_path, &migration.source_pool).await
                else {
                    return self
                        .finish_run(
                            run,
                            "deferred",
                            Some("source_pool_capacity_unavailable"),
                            heat_deferrals,
                        )
                        .await;
                };
                if used * 100 <= total * u128::from(low) {
                    return self
                        .finish_run(
                            run,
                            "finished",
                            Some("source_pool_low_watermark_reached"),
                            heat_deferrals,
                        )
                        .await;
                }
            }
            if !dispatch.admission.window_open(Utc::now()) {
                return self
                    .finish_run(run, "deferred", Some("admission_window_closed"), heat_deferrals)
                    .await;
            }
            if let Some(limits) = &policy.failure_pause {
                let (failures, terminal): (u64, u64) = sqlx::query_as(
                    "SELECT CAST(COUNT(CASE WHEN state = 'failed' THEN 1 END) AS UNSIGNED),
                            CAST(COUNT(*) AS UNSIGNED)
                     FROM policy_operations WHERE run_id = ? AND state IN ('failed','completed')
                     AND (finished_at >= (SELECT MAX(occurred_at) FROM policy_run_controls
                        WHERE run_id = ? AND action = 'resume')
                       OR NOT EXISTS (SELECT 1 FROM policy_run_controls WHERE run_id = ? AND action = 'resume'))",
                )
                .bind(run.id)
                .bind(run.id)
                .bind(run.id)
                .fetch_one(&self.pool)
                .await?;
                if failures >= limits.min_failures.get()
                    && u128::from(failures) * 100 >= u128::from(terminal) * u128::from(limits.percent)
                {
                    return self
                        .finish_run(run, "paused", Some("error_rate_limit"), heat_deferrals)
                        .await;
                }
            }
            if self.breaker_open(run.policy_id, &dispatch.admission.profile).await? {
                return self
                    .finish_run(run, "deferred", Some("policy_circuit_breaker_open"), heat_deferrals)
                    .await;
            }
            let invocation = policy.actions[candidate.branch as usize - 1].clone();
            let stats: Vec<Usage> = sqlx::query_as(
                "SELECT action, CAST(SUM(state <> 'deferred') AS UNSIGNED) AS operations,
                 CAST(SUM(COALESCE(attempt_count, CASE WHEN attempted OR state = 'submitting' OR attempts_reserved > 1
                   THEN attempts_reserved ELSE 0 END)) AS UNSIGNED) AS attempts,
                 CAST(SUM(CASE WHEN state <> 'deferred' THEN size ELSE 0 END) AS UNSIGNED) AS bytes
                 FROM policy_operations WHERE run_id = ? GROUP BY action",
            )
            .bind(run.id)
            .fetch_all(&self.pool)
            .await?;
            if usage_for_scope(&stats, policy.filesystem.as_str(), None).exhausted(policy.budget, elapsed_ms(run)) {
                return self
                    .finish_run(run, "finished", Some("policy_run_budget"), heat_deferrals)
                    .await;
            }
            let block = self
                .block_reason(&dispatch, invocation.kind, candidate.size, &stats)
                .await?;
            if block.as_ref().is_none_or(AdmissionBlock::is_capacity)
                && self
                    .awaiting_native_predecessor(policy.filesystem.as_str(), &candidate.fid, invocation.kind)
                    .await?
            {
                awaiting_predecessor = true;
                continue;
            }
            if let Some(block) = block {
                if (policy.one_shot || invocation.kind == ActionKind::PoolMigrate) && block.is_capacity() {
                    return self.finish_run(run, "running", Some(block.reason()), None).await;
                }
                self.record_deferred(&dispatch, candidate, &invocation, block.reason())
                    .await?;
                continue;
            }
            if let ControlFlow::Break(reason) = self
                .admit_candidate(&dispatch, candidate, invocation, &stats, worker)
                .await?
            {
                // Revisit this page on the next tick. Recorded candidates
                // are idempotent and heat counts move with the cursor.
                return self.finish_run(run, "running", Some(reason), None).await;
            }
        }
        if awaiting_predecessor {
            return self
                .finish_run(run, "running", Some("native_hsm_reconciliation_pending"), None)
                .await;
        }
        // A stale batch without submissions must not consume a one-shot that
        // was rescheduled when its bound safety revision changed.
        let mut tx = self.pool.begin().await?;
        let (state, reason) = if !current_revision(&mut tx, run).await? {
            ("cancelled", Some("policy_revision_inactive"))
        } else if through.is_none() && self.has_pending_migrations(run.id).await? {
            ("running", Some("migration_reconciliation_pending"))
        } else if through.is_none() {
            ("finished", None)
        } else {
            ("running", None)
        };
        sqlx::query(
            "UPDATE policy_runs
             SET cursor_id = ?, cursor_value = ?, state = ?, reason = ?,
                 heat_deferred_by_reason = COALESCE(?, heat_deferred_by_reason), updated_at = UTC_TIMESTAMP(6)
             WHERE id = ?",
        )
        .bind(through)
        .bind(page_end.and_then(|row| row.sort_value))
        .bind(state)
        .bind(reason)
        .bind(heat_deferrals)
        .bind(run.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn has_pending_migrations(&self, id: u64) -> Result<bool, PolicyError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM policy_operations WHERE run_id = ? AND pending_migration_fs IS NOT NULL)",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    fn pool_job_limit(&self) -> u32 {
        self.pool.options().get_max_connections().saturating_sub(3).max(1)
    }

    fn filesystem_migration_limit(&self, dispatch: &Dispatch<'_>) -> u16 {
        let requested = dispatch
            .policy
            .pool_migration()
            .and_then(|migration| migration.max_concurrency)
            .unwrap_or(1);
        u64::from(requested)
            .min(dispatch.admission.profile.slots)
            .min(u64::from(self.pool_job_limit()))
            .max(1) as u16
    }

    async fn block_reason(
        &self, dispatch: &Dispatch<'_>, action: ActionKind, size: u64, stats: &[Usage],
    ) -> Result<Option<AdmissionBlock>, PolicyError> {
        let elapsed = elapsed_ms(dispatch.run);
        let sum = |scope| usage_for_scope(stats, dispatch.policy.filesystem.as_str(), scope);
        if sum(None).exceeds(dispatch.policy.budget, size, elapsed) {
            return Ok(Some(AdmissionBlock::PolicyBudget));
        }
        for safety in &dispatch.admission.safety {
            if !safety_matches(safety, dispatch.policy.filesystem.as_str(), action) {
                continue;
            }
            if sum(Some(safety)).exceeds(BudgetLimits::from_safety(safety), size, elapsed) {
                return Ok(Some(AdmissionBlock::SafetyBudget));
            }
            if let Some(max) = safety["limits"]["maxUnknownOutstanding"].as_u64() {
                let rows: Vec<Outstanding> = sqlx::query_as(
                    "SELECT filesystem, action, COUNT(*) AS operations FROM policy_operations
                     WHERE pending_fid IS NOT NULL GROUP BY filesystem, action",
                )
                .fetch_all(&self.pool)
                .await?;
                let pending: u64 = rows
                    .iter()
                    .filter(|row| {
                        ActionKind::from_name(&row.action)
                            .is_some_and(|action| safety_matches(safety, &row.filesystem, action))
                    })
                    .map(|row| row.operations as u64)
                    .sum();
                if pending >= max {
                    return Ok(Some(AdmissionBlock::OutstandingLimit));
                }
            }
        }
        if action == ActionKind::PoolMigrate {
            let (active, previous_limit): (u64, Option<u64>) = sqlx::query_as(
                "SELECT CAST(COUNT(*) AS UNSIGNED),
                 MIN(CAST(COALESCE(JSON_VALUE(migration_plan, '$.filesystemConcurrency'), 1) AS UNSIGNED))
                 FROM policy_operations WHERE pending_migration_fs = ?",
            )
            .bind(dispatch.policy.filesystem.as_str())
            .fetch_one(&self.pool)
            .await?;
            let limit = u64::from(self.filesystem_migration_limit(dispatch)).min(previous_limit.unwrap_or(u64::MAX));
            if active >= limit || self.pool_jobs.lock().await.len() >= self.pool_job_limit() as usize {
                return Ok(Some(AdmissionBlock::MigrationBusy));
            }
        }
        let profile = &dispatch.admission.profile;
        let active = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM policy_operations WHERE execution_profile = ? AND pending_fid IS NOT NULL",
        )
        .bind(&profile.name)
        .fetch_one(&self.pool)
        .await?;
        if active as u64 >= profile.slots {
            return Ok(Some(AdmissionBlock::ProfileLimit));
        }
        Ok(None)
    }

    async fn breaker_open(&self, id: u64, profile: &Profile) -> Result<bool, PolicyError> {
        if profile.breakers.is_empty() {
            return Ok(false);
        }
        let (reset_at, open) = sqlx::query_as::<_, (Option<DateTime<Utc>>, bool)>(
            "SELECT reset_at, opened FROM policy_breakers WHERE policy_id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or_default();
        if open {
            return Ok(true);
        }
        for breaker in &profile.breakers {
            // The native adapter currently has two definitive error classes.
            let first = breaker.classes[0].name();
            let second = breaker.classes.get(1).map_or(first, |class| class.name());
            // Accepted requests that fail later are reconciliation outcomes, outside the submission streak.
            let last_other: Option<(DateTime<Utc>, u64)> = sqlx::query_as(
                "SELECT o.finished_at, o.id FROM policy_operations o JOIN policy_runs r ON r.id = o.run_id
                 WHERE r.policy_id = ? AND o.finished_at IS NOT NULL AND (? IS NULL OR o.finished_at > ?)
                 AND NOT (o.state = 'failed' AND o.accepted_at IS NOT NULL)
                 AND (o.terminal_error_class IS NULL OR o.terminal_error_class NOT IN (?, ?))
                 ORDER BY o.finished_at DESC, o.id DESC LIMIT 1",
            )
            .bind(id)
            .bind(reset_at)
            .bind(reset_at)
            .bind(first)
            .bind(second)
            .fetch_optional(&self.pool)
            .await?;
            let (finished_at, operation_id) = last_other.map_or((None, 0), |(time, id)| (Some(time), id));
            let failures = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM policy_operations o JOIN policy_runs r ON r.id = o.run_id
                 WHERE r.policy_id = ? AND (? IS NULL OR o.finished_at > ?) AND o.terminal_error_class IN (?, ?)
                 AND (? IS NULL OR (o.finished_at, o.id) > (?, ?))",
            )
            .bind(id)
            .bind(reset_at)
            .bind(reset_at)
            .bind(first)
            .bind(second)
            .bind(finished_at)
            .bind(finished_at)
            .bind(operation_id)
            .fetch_one(&self.pool)
            .await?;
            if failures as u64 >= breaker.threshold {
                sqlx::query(
                    "INSERT INTO policy_breakers (policy_id, reset_at, opened) VALUES (?, ?, TRUE)
                     ON DUPLICATE KEY UPDATE opened = IF(reset_at <=> VALUES(reset_at), TRUE, opened)",
                )
                .bind(id)
                .bind(reset_at)
                .execute(&self.pool)
                .await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn admit_candidate(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: Invocation, stats: &[Usage],
        worker: &mut Transaction<'_, MySql>,
    ) -> Result<ControlFlow<&'static str>, PolicyError> {
        let fs = dispatch.policy.filesystem.as_str();
        let in_flight =
            sqlx::query_scalar::<_, u64>("SELECT id FROM policy_operations WHERE filesystem = ? AND pending_fid = ?")
                .bind(fs)
                .bind(&candidate.fid)
                .fetch_optional(&self.pool)
                .await?;
        if in_flight.is_some() {
            return self
                .record_deferred(dispatch, candidate, &invocation, "unresolved_operation")
                .await
                .map(ControlFlow::Continue);
        }
        if let Some(migration) = &invocation.migration {
            let mut attempts = dispatch
                .policy
                .budget
                .attempts
                .saturating_sub(usage_for_scope(stats, fs, None).attempts)
                .min(u64::from(migration.retries.unwrap_or(0)) + 1);
            for safety in &dispatch.admission.safety {
                if safety_matches(safety, fs, invocation.kind) {
                    attempts = attempts.min(
                        BudgetLimits::from_safety(safety)
                            .attempts
                            .saturating_sub(usage_for_scope(stats, fs, Some(safety)).attempts),
                    );
                }
            }
            return self
                .admit_pool_migration(dispatch, candidate, &invocation, attempts as u32, worker)
                .await;
        }
        self.admit_native(dispatch, candidate, invocation, worker).await
    }

    async fn reserve_operation(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: &Invocation, generation: &Generation,
        migration: Option<MigrationReservation<'_>>, store: impl MySqlExecutor<'_>,
    ) -> Result<u64, PolicyError> {
        let runtime = self.runtime.as_ref().expect("dispatch requires a runtime");
        Ok(sqlx::query(
            "INSERT INTO policy_operations (run_id, filesystem, fid, action, archive_id, state, generation, size,
                 principal, execution_profile, migration_plan, paths, attempts_reserved, migration_lease_until)
             VALUES (?, ?, ?, ?, ?, 'submitting', ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(dispatch.run.id)
        .bind(dispatch.policy.filesystem.as_str())
        .bind(&candidate.fid)
        .bind(invocation.kind.name())
        .bind(invocation.archive_id)
        .bind(Json(generation))
        .bind(generation.size)
        .bind(Json(&runtime.principal))
        .bind(&dispatch.admission.profile.name)
        .bind(migration.as_ref().map(|migration| Json(migration.plan)))
        .bind(
            migration
                .as_ref()
                .filter(|migration| !migration.paths.is_empty())
                .map(|migration| Json(migration.paths)),
        )
        .bind(migration.as_ref().map_or(1, |migration| migration.attempts))
        .bind(migration.map(|migration| migration.lease_until))
        .execute(store)
        .await?
        .last_insert_id())
    }

    async fn record_deferred(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: &Invocation, reason: &str,
    ) -> Result<(), PolicyError> {
        sqlx::query(
            "INSERT INTO policy_operations (run_id, filesystem, fid, action, archive_id, state, reason, size)
             VALUES (?, ?, ?, ?, ?, 'deferred', ?, ?) ON DUPLICATE KEY UPDATE id = id",
        )
        .bind(dispatch.run.id)
        .bind(dispatch.policy.filesystem.as_str())
        .bind(&candidate.fid)
        .bind(invocation.kind.name())
        .bind(invocation.archive_id)
        .bind(short_reason(reason))
        .bind(candidate.size)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn reconcile_operations(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        self.reap_pool_jobs().await;
        self.finish_native_submissions().await;
        self.finish_pool_recoveries(entries).await?;
        self.finish_native_recoveries().await;
        self.start_pending_pool_recoveries(entries).await?;
        self.start_pending_native_recoveries(entries).await
    }
}

fn exclude_recovery_ids(query: &mut QueryBuilder<'_, MySql>, ids: impl Iterator<Item = u64>) {
    let mut excluded = query.push(" AND o.id NOT IN (").separated(", ");
    for id in ids {
        excluded.push_bind(id);
    }
    excluded.push_unseparated(")");
}

async fn lock_permit(tx: &mut Transaction<'_, MySql>, operation: u64) -> Result<bool, PolicyError> {
    Ok(sqlx::query_scalar::<_, u64>(
        "SELECT id FROM policy_operations WHERE id = ? AND state = 'submitting' FOR UPDATE",
    )
    .bind(operation)
    .fetch_optional(&mut **tx)
    .await?
    .is_some())
}

async fn verify_worker_permit(worker: &mut Transaction<'_, MySql>) -> Result<bool, PolicyError> {
    // The committed intent still reserves the FID and budget across a loss of this lock.
    Ok(sqlx::query_scalar::<_, u8>(
        "SELECT id FROM policy_worker_lock WHERE id = 1 AND @@GLOBAL.innodb_flush_log_at_trx_commit = 1 FOR UPDATE",
    )
    .fetch_optional(&mut **worker)
    .await?
    .is_some())
}

async fn existing_one_shot_run(
    tx: &mut Transaction<'_, MySql>, id: u64, revision: u64, definition: &Json<Value>,
) -> Result<Option<u64>, PolicyError> {
    // Safety policy changes create revisions for the same business definition.
    // A run that has already evaluated or dispatched work stays consumed.
    Ok(sqlx::query_scalar(
        "SELECT r.id FROM policy_runs r JOIN policy_revisions rev
         ON rev.policy_id = r.policy_id AND rev.revision = r.revision
         WHERE r.policy_id = ? AND (r.revision = ? OR (
           BINARY rev.definition = BINARY ? AND (
             r.state IN ('previewed','finished') OR EXISTS (
               SELECT 1 FROM policy_operations o WHERE o.run_id = r.id AND o.state <> 'deferred'
             )
           )
         )) ORDER BY r.id LIMIT 1",
    )
    .bind(id)
    .bind(revision)
    .bind(definition)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn current_revision(tx: &mut Transaction<'_, MySql>, run: &PolicyRun) -> Result<bool, PolicyError> {
    let revision = sqlx::query_scalar::<_, u64>(
        "SELECT revision FROM policies WHERE id = ? AND enabled AND name IS NOT NULL LOCK IN SHARE MODE",
    )
    .bind(run.policy_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(revision == Some(run.revision))
}

fn elapsed_ms(run: &PolicyRun) -> u64 {
    ((Utc::now() - run.started_at.expect("run started"))
        .num_milliseconds()
        .max(0) as u64)
        .saturating_sub(run.paused_millis)
}

fn usage_for_scope(stats: &[Usage], filesystem: &str, scope: Option<&Value>) -> Usage {
    let mut sum = Usage::default();
    for usage in stats {
        if scope.is_none_or(|scope| {
            ActionKind::from_name(&usage.action).is_some_and(|kind| safety_matches(scope, filesystem, kind))
        }) {
            sum.add(usage);
        }
    }
    sum
}

fn short_reason(reason: &str) -> String {
    reason.chars().take(255).collect()
}
