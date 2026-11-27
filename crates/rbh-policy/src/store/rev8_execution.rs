mod migration;
pub(super) use migration::PoolRecovery;

use std::{
    collections::BTreeMap,
    ops::ControlFlow,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use lustre_api::{HsmState, LuFid, migration::MigrationPlan};
use rbh_entry_store::{EntryStore, FileSystemConfig, fid_codec};
use serde::Serialize;
use serde_json::Value;
use sqlx::{MySql, QueryBuilder, Transaction, types::Json};

use super::PolicyStore;
use crate::{
    PolicyError,
    rev8::{
        self, ActionKind, BudgetLimits, CompiledPolicy, DB_EPOCH_MICROS, Invocation,
        execution::{
            Admission, Decision, FidObservation, Generation, LustreHsm, NativeHsm, Profile, Submission,
            SubmissionDeferral, SubmissionGate,
        },
        heat::{self, HeatProfile},
        invalid, push_expression,
        resources::safety_matches,
    },
};

const BATCH_SIZE: usize = 64;
const RUN_FIELDS: &str =
    "id, policy_id, revision, state, reason, candidate_count, estimated_bytes, requested_by, created_at, started_at,
     heat_deferred_by_reason";
const OPERATION_FIELDS: &str =
    "o.id, o.run_id, r.policy_id, r.revision, o.filesystem, o.fid, o.action, o.archive_id, o.state,
     o.reason, o.attempted, o.generation, o.principal, o.size, o.accepted_at, o.completion_evidence, o.migration_plan,
     o.checked_at";

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Rev8Run {
    pub id: u64,
    pub policy_id: u64,
    pub revision: u64,
    pub state: String,
    pub reason: Option<String>,
    pub candidate_count: Option<u64>,
    pub estimated_bytes: Option<u64>,
    pub requested_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heat_deferred_by_reason: Option<Json<BTreeMap<String, u64>>>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Rev8Operation {
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

#[derive(sqlx::FromRow)]
struct Candidate {
    fid: Vec<u8>,
    size: u64,
    mtime: i64,
    ctime: i64,
    branch: i64,
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
    run: &'a Rev8Run,
    policy: &'a CompiledPolicy,
    admission: &'a Admission,
    config: &'a FileSystemConfig,
    heat: Option<&'a HeatProfile>,
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
    pub async fn request_rev8_run(&self, id: u64, actor: &str) -> Result<u64, PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let row: Option<(u64, Json<Value>)> = sqlx::query_as(
            "SELECT revision, definition FROM policies WHERE id = ? AND revision IS NOT NULL AND name IS NOT NULL FOR UPDATE",
        ).bind(id).fetch_optional(&mut *tx).await?;
        let (revision, definition) = row.ok_or(PolicyError::NotFound(id))?;
        if definition["spec"]["enabled"] == false {
            return Err(invalid("the policy is disabled"));
        }
        if definition["spec"]["mode"] == "one_shot"
            && let Some(existing) = existing_one_shot_run(&mut tx, id, revision, &definition).await?
        {
            return Ok(existing);
        }
        let run = sqlx::query("INSERT INTO rev8_runs (policy_id, revision, requested_by) VALUES (?, ?, ?)")
            .bind(id)
            .bind(revision)
            .bind(actor)
            .execute(&mut *tx)
            .await?
            .last_insert_id();
        tx.commit().await?;
        Ok(run)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_rev8_runs(&self, id: u64, after: u64) -> Result<Vec<Rev8Run>, PolicyError> {
        let exists = sqlx::query_scalar::<_, u64>("SELECT id FROM policies WHERE id = ? AND revision IS NOT NULL")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            return Err(PolicyError::NotFound(id));
        }
        Ok(sqlx::query_as(&format!(
            "SELECT {RUN_FIELDS} FROM rev8_runs WHERE policy_id = ? AND id > ? ORDER BY id LIMIT 100"
        ))
        .bind(id)
        .bind(after)
        .fetch_all(&self.pool)
        .await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn list_rev8_operations(
        &self, policy: u64, run: u64, after: u64,
    ) -> Result<Vec<Rev8Operation>, PolicyError> {
        let exists = sqlx::query_scalar::<_, u64>("SELECT id FROM rev8_runs WHERE id = ? AND policy_id = ?")
            .bind(run)
            .bind(policy)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            return Err(PolicyError::NotFound(run));
        }
        Ok(sqlx::query_as(&format!(
            "SELECT {OPERATION_FIELDS} FROM rev8_operations o JOIN rev8_runs r ON r.id = o.run_id
            WHERE r.id = ? AND o.id > ? ORDER BY o.id LIMIT 100"
        ))
        .bind(run)
        .bind(after)
        .fetch_all(&self.pool)
        .await?)
    }

    #[tracing::instrument(skip(self))]
    pub async fn reset_rev8_breaker(&self, id: u64, actor: &str) -> Result<(), PolicyError> {
        let mut tx = self.lock_configuration().await?;
        let active = sqlx::query_scalar::<_, u64>(
            "SELECT id FROM policies WHERE id = ? AND revision IS NOT NULL AND name IS NOT NULL FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        if active.is_none() {
            return Err(PolicyError::NotFound(id));
        }
        sqlx::query(
            "INSERT INTO rev8_breakers (policy_id, reset_by, reset_at)
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
    #[tracing::instrument(skip_all, fields(worker = "rev8"))]
    pub async fn process_rev8(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        let mut worker = self.pool.begin().await?;
        sqlx::query_scalar::<_, u8>("SELECT id FROM rev8_worker_lock WHERE id = 1 FOR UPDATE")
            .fetch_one(&mut *worker)
            .await?;

        self.reconcile_operations(entries).await?;
        self.enqueue_due_policies().await?;
        let run: Option<Rev8Run> = sqlx::query_as(&format!(
            "SELECT {RUN_FIELDS} FROM rev8_runs WHERE state IN ('queued', 'running') ORDER BY updated_at, id LIMIT 1"
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
             WHERE p.enabled AND p.rev8_next_run <= UTC_TIMESTAMP(6) AND NOT EXISTS (
                SELECT 1 FROM rev8_runs r WHERE r.policy_id = p.id AND r.revision = p.revision
                AND r.state IN ('queued','running'))
             ORDER BY p.rev8_next_run, p.id LIMIT 64 FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (id, revision, definition) in due {
            let policy = rev8::parse(&definition)?;
            let next = policy.next_run(Utc::now())?;
            sqlx::query("UPDATE policies SET rev8_next_run = ? WHERE id = ?")
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
            sqlx::query("INSERT INTO rev8_runs (policy_id, revision) VALUES (?, ?)")
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
        &self, entries: &EntryStore, mut run: Rev8Run, worker: &mut Transaction<'_, MySql>,
    ) -> Result<(), PolicyError> {
        let Some(row) = self.get_rev8(run.policy_id).await? else {
            return self.finish_run(&run, "cancelled", Some("policy_deleted"), None).await;
        };
        if row.revision != run.revision || row.definition["spec"]["enabled"] == false {
            return self
                .finish_run(&run, "cancelled", Some("policy_revision_inactive"), None)
                .await;
        }
        let policy = rev8::parse(&row.definition)?;
        if self.ensure_configured(policy.filesystem()).is_err() {
            return self
                .finish_run(&run, "deferred", Some("filesystem_not_configured"), None)
                .await;
        }
        let bindings = row.resource_bindings.map(|bindings| bindings.0).unwrap_or_default();
        let policy = self.compile_revision(policy, &bindings).await?;
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
            let (state, reason) = if !active {
                ("cancelled", Some("policy_revision_inactive"))
            } else if policy.enforced {
                ("running", None)
            } else {
                ("previewed", None)
            };
            sqlx::query(
                "UPDATE rev8_runs SET state = ?, reason = ?, candidate_count = ?, estimated_bytes = ?, started_at = ?,
                 heat_deferred_by_reason = ? WHERE id = ?",
            )
            .bind(state)
            .bind(reason)
            .bind(preview.candidate_count)
            .bind(preview.estimated_bytes)
            .bind(now)
            .bind(preview.read_heat.map(|heat| Json(heat.deferred_by_reason)))
            .bind(run.id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            if !active || !policy.enforced {
                return Ok(());
            }
        }
        run.started_at.get_or_insert(now);
        let Some(runtime) = self.runtime.as_ref() else {
            return self
                .finish_run(&run, "deferred", Some("workload_identity_unavailable"), None)
                .await;
        };
        let config = match rev8::validate_filesystem(entries, &policy.filesystem).await {
            Ok(config) => config,
            Err(PolicyError::InvalidPolicy(reason)) => {
                return self.finish_run(&run, "deferred", Some(&reason), None).await;
            }
            Err(error) => return Err(error),
        };
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
            admission: &admission,
            config: &config,
            heat,
        };
        self.dispatch_batch(dispatch, worker).await
    }

    async fn finish_run(
        &self, run: &Rev8Run, state: &str, reason: Option<&str>, heat_deferrals: Option<Json<BTreeMap<String, u64>>>,
    ) -> Result<(), PolicyError> {
        let mut tx = self.pool.begin().await?;
        let (state, reason) = if state != "cancelled" && !current_revision(&mut tx, run).await? {
            ("cancelled", Some("policy_revision_inactive"))
        } else {
            (state, reason)
        };
        sqlx::query(
            "UPDATE rev8_runs SET state = ?, reason = ?, updated_at = UTC_TIMESTAMP(6),
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
        let cursor: Option<Vec<u8>> = sqlx::query_scalar("SELECT cursor_id FROM rev8_runs WHERE id = ?")
            .bind(run.id)
            .fetch_one(&self.pool)
            .await?;
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
        let mut query = QueryBuilder::new("SELECT entries.object_id AS fid, size, mtime, ctime, CAST((");
        push_expression(&mut query, &policy.decision, &policy.decision_params);
        query.push(") AS SIGNED) AS branch FROM scoped_entries AS entries ");
        if let Some(profile) = dispatch.heat {
            heat::push_scores(&mut query, profile, now, None);
        }
        query
            .push(" WHERE entries.filesystem_id = ")
            .push_bind(policy.filesystem.as_str())
            .push(" AND entries.object_kind = 0 AND entries.object_id > ")
            .push_bind(after)
            .push(" AND (");
        policy.push_scope(&mut query, database_time_micros);
        query.push(format!(
            ") HAVING branch > 0 ORDER BY entries.object_id LIMIT {BATCH_SIZE}"
        ));
        let select = query.build_query_as::<Candidate>();
        let candidates = match snapshot.as_mut() {
            Some(tx) => select.fetch_all(&mut **tx).await?,
            None => select.fetch_all(&self.pool).await?,
        };
        let through = candidates
            .last()
            .filter(|_| candidates.len() == BATCH_SIZE)
            .map(|row| row.fid.as_slice());
        let mut heat_deferrals = None;
        if let (Some(mut tx), Some(profile)) = (snapshot, dispatch.heat) {
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
        for candidate in &candidates {
            if !dispatch.admission.window_open(Utc::now()) {
                return self
                    .finish_run(run, "deferred", Some("admission_window_closed"), heat_deferrals)
                    .await;
            }
            if self.breaker_open(run.policy_id, &dispatch.admission.profile).await? {
                return self
                    .finish_run(run, "deferred", Some("policy_circuit_breaker_open"), heat_deferrals)
                    .await;
            }
            let invocation = policy.actions[candidate.branch as usize - 1].clone();
            let stats: Vec<Usage> = sqlx::query_as(
                "SELECT action, CAST(COUNT(*) AS UNSIGNED) AS operations, CAST(SUM(attempted OR state = 'submitting') AS UNSIGNED) AS attempts,
                 CAST(SUM(size) AS UNSIGNED) AS bytes FROM rev8_operations WHERE run_id = ? AND state <> 'deferred' GROUP BY action",
            ).bind(run.id).fetch_all(&self.pool).await?;
            if usage_for_scope(&stats, policy.filesystem.as_str(), None).exhausted(policy.budget, elapsed_ms(run)) {
                return self
                    .finish_run(run, "finished", Some("policy_run_budget"), heat_deferrals)
                    .await;
            }
            if let Some(block) = self
                .block_reason(&dispatch, invocation.kind, candidate.size, &stats)
                .await?
            {
                if policy.one_shot && block.is_capacity() {
                    return self.finish_run(run, "running", Some(block.reason()), None).await;
                }
                self.record_deferred(&dispatch, candidate, &invocation, block.reason())
                    .await?;
                continue;
            }
            self.admit_candidate(&dispatch, candidate, invocation, worker).await?;
        }
        // A stale batch without submissions must not consume a one-shot that
        // was rescheduled when its bound safety revision changed.
        let mut tx = self.pool.begin().await?;
        let (state, reason) = if !current_revision(&mut tx, run).await? {
            ("cancelled", Some("policy_revision_inactive"))
        } else if through.is_none() {
            ("finished", None)
        } else {
            ("running", None)
        };
        sqlx::query(
            "UPDATE rev8_runs
             SET cursor_id = ?, state = ?, reason = ?,
                 heat_deferred_by_reason = COALESCE(?, heat_deferred_by_reason), updated_at = UTC_TIMESTAMP(6)
             WHERE id = ?",
        )
        .bind(through)
        .bind(state)
        .bind(reason)
        .bind(heat_deferrals)
        .bind(run.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
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
                    "SELECT filesystem, action, COUNT(*) AS operations FROM rev8_operations
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
            let occupied =
                sqlx::query_scalar::<_, u64>("SELECT id FROM rev8_operations WHERE pending_migration_fs = ?")
                    .bind(dispatch.policy.filesystem.as_str())
                    .fetch_optional(&self.pool)
                    .await?;
            if occupied.is_some() {
                return Ok(Some(AdmissionBlock::MigrationBusy));
            }
        }
        let profile = &dispatch.admission.profile;
        let active = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM rev8_operations WHERE execution_profile = ? AND pending_fid IS NOT NULL",
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
            "SELECT reset_at, opened FROM rev8_breakers WHERE policy_id = ?",
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
                "SELECT o.finished_at, o.id FROM rev8_operations o JOIN rev8_runs r ON r.id = o.run_id
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
                "SELECT COUNT(*) FROM rev8_operations o JOIN rev8_runs r ON r.id = o.run_id
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
                    "INSERT INTO rev8_breakers (policy_id, reset_at, opened) VALUES (?, ?, TRUE)
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
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, mut invocation: Invocation,
        worker: &mut Transaction<'_, MySql>,
    ) -> Result<(), PolicyError> {
        let fs = dispatch.policy.filesystem.as_str();
        let existing = sqlx::query_scalar::<_, u64>("SELECT id FROM rev8_operations WHERE run_id = ? AND fid = ?")
            .bind(dispatch.run.id)
            .bind(&candidate.fid)
            .fetch_optional(&self.pool)
            .await?;
        if existing.is_some() {
            return Ok(());
        }
        let in_flight =
            sqlx::query_scalar::<_, u64>("SELECT id FROM rev8_operations WHERE filesystem = ? AND pending_fid = ?")
                .bind(fs)
                .bind(&candidate.fid)
                .fetch_optional(&self.pool)
                .await?;
        if in_flight.is_some() {
            return self
                .record_deferred(dispatch, candidate, &invocation, "unresolved_operation")
                .await;
        }
        let fid = decode_fid(&candidate.fid)?;
        let mount = &dispatch.config.mount_path;
        let snapshot = match self.backend().observe(mount, fid).await {
            Ok(FidObservation::Present(snapshot)) => snapshot,
            Ok(FidObservation::Missing { .. }) => {
                return self
                    .record_deferred(dispatch, candidate, &invocation, "fid_missing")
                    .await;
            }
            Err(error) => return self.record_deferred(dispatch, candidate, &invocation, &error).await,
        };
        let seen = &snapshot.generation;
        if seen.size != candidate.size || seen.mtime != candidate.mtime || seen.ctime != candidate.ctime {
            return self
                .record_deferred(dispatch, candidate, &invocation, "catalog_generation_changed")
                .await;
        }
        if matches!(invocation.kind, ActionKind::Release | ActionKind::Restore) {
            invocation.archive_id = snapshot.state.archive_id;
        }
        if let Decision::Defer(reason) = dispatch.admission.authorize(fs, &invocation) {
            return self.record_deferred(dispatch, candidate, &invocation, &reason).await;
        }
        if let Err(reason) = invocation.precondition(&snapshot.state) {
            return self.record_deferred(dispatch, candidate, &invocation, reason).await;
        }
        if invocation.migration.is_some() {
            return self
                .admit_pool_migration(dispatch, candidate, &invocation, seen, worker)
                .await;
        }
        let operation = self
            .reserve_operation(dispatch, candidate, &invocation, seen, None)
            .await?;

        // Hold the policy head through revalidation and the request. A disable,
        // replacement or delete cannot commit in the middle of issuing it.
        let mut tx = self.pool.begin().await?;
        let active = current_revision(&mut tx, dispatch.run).await?;
        if !lock_permit(&mut tx, operation).await? {
            return Ok(());
        }
        let (state, attempted, reason, terminal_class) = if !active || !dispatch.admission.window_open(Utc::now()) {
            ("deferred", false, Some("policy_or_window_inactive".into()), None)
        } else {
            let live = self.backend().observe(mount, fid).await;
            match live {
                Ok(FidObservation::Present(current))
                    if current.generation == snapshot.generation && current.state == snapshot.state =>
                {
                    let gate = native_submission_gate(dispatch, invocation.kind);
                    if let ControlFlow::Break(reason) = gate.check() {
                        ("deferred", false, Some(reason.reason().into()), None)
                    } else {
                        // Verify that the worker transaction still owns its lock.
                        // The committed operation reserves the FID and budget even
                        // if the connection is lost after this check.
                        if !verify_worker_permit(worker).await? {
                            (
                                "deferred",
                                false,
                                Some("permit_store_requires_synchronous_commit".into()),
                                None,
                            )
                        } else {
                            match self.backend().submit(mount, fid, invocation, seen, gate).await {
                                Submission::Accepted => ("submitted", true, None, None),
                                Submission::Deferred(reason) => ("deferred", false, Some(reason.reason().into()), None),
                                Submission::Rejected { error, class } => {
                                    ("failed", true, Some(error), class.map(|class| class.name()))
                                }
                                Submission::Unknown(error) => ("unknown", true, Some(error), None),
                            }
                        }
                    }
                }
                _ => ("deferred", false, Some("target_changed_before_submit".into()), None),
            }
        };
        sqlx::query(
            "UPDATE rev8_operations SET state = ?, attempted = ?, reason = ?, terminal_error_class = ?,
             accepted_at = CASE WHEN ? = 'submitted' THEN UTC_TIMESTAMP(6) ELSE NULL END,
             finished_at = CASE WHEN ? = 'failed' THEN UTC_TIMESTAMP(6) ELSE NULL END WHERE id = ? AND state = 'submitting'",
        ).bind(state).bind(attempted).bind(reason.as_deref().map(short_reason)).bind(terminal_class)
            .bind(state).bind(state).bind(operation).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn reserve_operation(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: &Invocation, generation: &Generation,
        migration: Option<&MigrationPlan>,
    ) -> Result<u64, PolicyError> {
        let runtime = self.runtime.as_ref().expect("dispatch requires a runtime");
        Ok(sqlx::query(
            "INSERT INTO rev8_operations (run_id, filesystem, fid, action, archive_id, state, generation, size,
                 principal, execution_profile, migration_plan)
             VALUES (?, ?, ?, ?, ?, 'submitting', ?, ?, ?, ?, ?)",
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
        .bind(migration.map(Json))
        .execute(&self.pool)
        .await?
        .last_insert_id())
    }

    async fn record_deferred(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: &Invocation, reason: &str,
    ) -> Result<(), PolicyError> {
        sqlx::query(
            "INSERT INTO rev8_operations (run_id, filesystem, fid, action, archive_id, state, reason, size)
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
        self.finish_pool_recoveries(entries).await?;
        let mut query = QueryBuilder::new(format!(
            "SELECT {OPERATION_FIELDS} FROM rev8_operations o JOIN rev8_runs r ON r.id = o.run_id
             WHERE o.pending_fid IS NOT NULL"
        ));
        let mut recovery_slots = {
            let recoveries = self.pool_recoveries.lock().await;
            let slots = BATCH_SIZE.saturating_sub(recoveries.len());
            if slots == 0 {
                query.push(" AND o.pending_migration_fs IS NULL");
            } else if !recoveries.is_empty() {
                let mut ids = query.push(" AND o.id NOT IN (").separated(", ");
                for id in recoveries.keys().copied() {
                    ids.push_bind(id);
                }
                ids.push_unseparated(")");
            }
            slots
        };
        query
            .push(" ORDER BY o.checked_at, o.id LIMIT ")
            .push_bind(BATCH_SIZE as u32);
        let pending: Vec<Rev8Operation> = query.build_query_as().fetch_all(&self.pool).await?;
        for operation in pending {
            let filesystem = rbh_entry_store::FileSystemId::new(&operation.filesystem)
                .map_err(|error| PolicyError::Store(error.to_string()))?;
            if operation.action == ActionKind::PoolMigrate.name() {
                if recovery_slots > 0 && self.start_pool_recovery(entries, filesystem, operation).await? {
                    recovery_slots -= 1;
                }
                continue;
            }
            let action =
                ActionKind::from_name(&operation.action).ok_or_else(|| invalid("unknown persisted HSM action"))?;
            let invocation = Invocation {
                kind: action,
                archive_id: operation.archive_id,
                requires_human: false,
                migration: None,
            };
            let fid = decode_fid(&operation.fid)?;
            let config = match rev8::validate_filesystem(entries, &filesystem).await {
                Ok(config) => Some(config),
                Err(PolicyError::InvalidPolicy(_)) => None,
                Err(error) => return Err(error),
            };
            let mut observation = None;
            let mut confirmed_idle = false;
            if let Some(config) = config {
                let backend = self.backend();
                let observe = || async {
                    backend.observe(&config.mount_path, fid).await.inspect_err(|error| {
                        tracing::debug!(%filesystem, %fid, operation_id = operation.id, %error, "HSM observation unavailable");
                    }).ok()
                };
                observation = observe().await;
                if operation.state == "submitted"
                    && let (Some(generation), Some(FidObservation::Present(observed))) =
                        (&operation.generation, &observation)
                    && generation.mount_source_id.is_some()
                    && generation.same_content(&observed.generation)
                    && observed.state.coordinator_idle()
                    && !invocation.completed(&observed.state)
                {
                    // The first flags could predate completion because the current
                    // action is queried last. Reread flags after the idle observation.
                    observation = observe().await;
                    confirmed_idle = true;
                }
            }
            let mut state = operation.state.as_str();
            let mut reason = operation.reason.as_deref();
            let mut evidence = None;
            match (&operation.generation, observation) {
                (Some(generation), Some(FidObservation::Present(observed)))
                    if generation.same_content(&observed.generation) =>
                {
                    let source = if invocation.completed(&observed.state) {
                        state = "completed";
                        reason = None;
                        Some("lustre_hsm_state")
                    } else if confirmed_idle
                        && observed.state.coordinator_idle()
                        && consistent_hsm_flags(observed.state.states)
                    {
                        state = "failed";
                        reason = Some("hsm_target_state_not_reached");
                        Some("lustre_hsm_current_action")
                    } else {
                        None
                    };
                    evidence = source.map(|source| {
                        Json(serde_json::json!({
                            "source": source, "observedAt": Utc::now(),
                            "state": observed.state, "generation": observed.generation,
                        }))
                    });
                }
                (Some(generation), Some(FidObservation::Missing { mount_source_id }))
                    if state == "submitted" && generation.mount_source_id.as_deref() == Some(&mount_source_id) =>
                {
                    state = "failed";
                    reason = Some("fid_missing");
                    evidence = Some(Json(serde_json::json!({
                        "source": "lustre_fid_lookup", "observedAt": Utc::now(), "mountSourceId": mount_source_id,
                    })));
                }
                _ => {}
            }
            // An interrupted ioctl may still be in progress even if the coordinator looks idle.
            if state == "submitting" {
                state = "unknown";
                reason = Some("interrupted_submission");
            }
            sqlx::query(
                "UPDATE rev8_operations SET state = ?, reason = ?, checked_at = UTC_TIMESTAMP(6),
                 finished_at = CASE WHEN ? IN ('completed', 'failed') THEN UTC_TIMESTAMP(6) ELSE NULL END,
                 completion_evidence = ?, attempted = TRUE WHERE id = ? AND state = ? AND pending_fid IS NOT NULL",
            )
            .bind(state)
            .bind(reason)
            .bind(state)
            .bind(evidence)
            .bind(operation.id)
            .bind(&operation.state)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    fn backend(&self) -> &dyn NativeHsm {
        self.runtime.as_ref().map_or(&LustreHsm, |runtime| runtime.hsm.as_ref())
    }
}

async fn lock_permit(tx: &mut Transaction<'_, MySql>, operation: u64) -> Result<bool, PolicyError> {
    Ok(
        sqlx::query_scalar::<_, u64>("SELECT id FROM rev8_operations WHERE id = ? AND state = 'submitting' FOR UPDATE")
            .bind(operation)
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
    )
}

async fn verify_worker_permit(worker: &mut Transaction<'_, MySql>) -> Result<bool, PolicyError> {
    // The committed intent still reserves the FID and budget across a loss of this lock.
    Ok(sqlx::query_scalar::<_, u8>(
        "SELECT id FROM rev8_worker_lock WHERE id = 1 AND @@GLOBAL.innodb_flush_log_at_trx_commit = 1 FOR UPDATE",
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
        "SELECT r.id FROM rev8_runs r JOIN rev8_policy_revisions rev
         ON rev.policy_id = r.policy_id AND rev.revision = r.revision
         WHERE r.policy_id = ? AND (r.revision = ? OR (
           BINARY rev.definition = BINARY ? AND (
             r.state IN ('previewed','finished') OR EXISTS (
               SELECT 1 FROM rev8_operations o WHERE o.run_id = r.id AND o.state <> 'deferred'
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

async fn current_revision(tx: &mut Transaction<'_, MySql>, run: &Rev8Run) -> Result<bool, PolicyError> {
    let revision = sqlx::query_scalar::<_, u64>(
        "SELECT revision FROM policies WHERE id = ? AND enabled AND revision IS NOT NULL AND name IS NOT NULL FOR UPDATE",
    ).bind(run.policy_id).fetch_optional(&mut **tx).await?;
    Ok(revision == Some(run.revision))
}

fn elapsed_ms(run: &Rev8Run) -> u64 {
    (Utc::now() - run.started_at.expect("run started"))
        .num_milliseconds()
        .max(0) as u64
}

fn consistent_hsm_flags(flags: HsmState) -> bool {
    use HsmState as S;
    HsmState::from_bits(flags.bits()).is_some()
        && (!flags.contains(S::DIRTY) || flags.contains(S::EXISTS) && !flags.contains(S::RELEASED))
        && (!flags.contains(S::RELEASED) || flags.contains(S::ARCHIVED))
        && (!flags.contains(S::LOST) || flags.contains(S::ARCHIVED))
}

fn native_submission_gate(dispatch: &Dispatch<'_>, action: ActionKind) -> SubmissionGate {
    // Anchor the persisted run age before blocking. A wall-clock step during
    // preparation must not extend the remaining duration budgets.
    let checked_at = Instant::now();
    let already_elapsed = elapsed_ms(dispatch.run);
    let remaining = |limit: u64| Duration::from_millis(limit.saturating_sub(already_elapsed));
    let window_open = dispatch.admission.window_check();
    let policy_limit = dispatch.policy.budget.duration_ms.map(remaining);
    let safety_limit = dispatch
        .admission
        .safety
        .iter()
        .filter(|scope| safety_matches(scope, dispatch.policy.filesystem.as_str(), action))
        .filter_map(|scope| BudgetLimits::from_safety(scope).duration_ms)
        .min()
        .map(remaining);
    SubmissionGate(Box::new(move || {
        if !window_open() {
            return ControlFlow::Break(SubmissionDeferral::WindowClosed);
        }
        let elapsed = checked_at.elapsed();
        if policy_limit.is_some_and(|limit| elapsed >= limit) {
            return ControlFlow::Break(SubmissionDeferral::PolicyRunDuration);
        }
        if safety_limit.is_some_and(|limit| elapsed >= limit) {
            return ControlFlow::Break(SubmissionDeferral::SystemSafetyRunDuration);
        }
        ControlFlow::Continue(())
    }))
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
