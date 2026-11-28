use std::{
    borrow::Cow,
    ops::ControlFlow,
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::Utc;
use lustre_api::{
    LuFid, LustreApi, StripeInfo,
    migration::{
        FileAccessTime, FileVersion, MigrationError, MigrationLimits, MigrationOwnership, MigrationPlacement,
        MigrationPlan, VerifiedPlacement,
    },
};
use rbh_entry_store::{BackendKind, EntryKey, EntryStore, FileSystemId, ObjectId};
use serde_json::json;
use sqlx::{MySql, QueryBuilder, Transaction, types::Json};

use super::{
    BATCH_SIZE, Candidate, Dispatch, Generation, Invocation, OPERATION_FIELDS, PolicyError, PolicyStore, Rev8Operation,
    current_revision, decode_fid, elapsed_ms, exclude_recovery_ids, lock_permit,
    preflight::{PreflightKey, PreflightRequest, PreflightResult, PreflightWait},
    short_reason, verify_worker_permit,
};
use crate::{
    Rev8TickTask,
    rev8::{
        ActionKind, AdmittedLinks, BudgetLimits, DirectoryScope,
        execution::{Decision, FidObservation, NativeHsm, SubmissionDeferral, SubmissionGate, lustre_call},
        heat, push_expression,
        resources::safety_matches,
    },
};

pub(in crate::store) type PoolPreflight = Result<PoolSource, Cow<'static, str>>;
const MIGRATION_DEADLINE_PENDING: &str = "migration_deadline_pending";

pub(in crate::store) struct PoolSource {
    generation: Generation,
    layout: StripeInfo,
    access_time: FileAccessTime,
    links: Option<AdmittedLinks>,
}

pub(in crate::store) struct PoolRecovery {
    operation: Rev8Operation,
    filesystem: FileSystemId,
    fid: LuFid,
    mount: PathBuf,
    expected: FileVersion,
    task: JoinHandle<(Result<MigrationPlacement, MigrationError>, MigrationOwnership)>,
}

impl PolicyStore {
    pub(super) async fn admit_pool_migration(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: &Invocation,
        worker: &mut Transaction<'_, MySql>,
    ) -> Result<ControlFlow<&'static str>, PolicyError> {
        let options = invocation.migration.as_ref().expect("pool migration invocation");
        if candidate.size > options.max_file_bytes {
            return self
                .record_deferred(dispatch, candidate, invocation, "migration_max_file_bytes")
                .await
                .map(ControlFlow::Continue);
        }
        let Some(deadline) = migration_deadline(dispatch) else {
            return self
                .record_deferred(dispatch, candidate, invocation, "migration_deadline_elapsed")
                .await
                .map(ControlFlow::Continue);
        };
        let filesystem = &dispatch.policy.filesystem;
        if let Decision::Defer(reason) = dispatch.admission.authorize(filesystem.as_str(), invocation) {
            return self
                .record_deferred(dispatch, candidate, invocation, &reason)
                .await
                .map(ControlFlow::Continue);
        }
        let fid = decode_fid(&candidate.fid)?;
        // Share one wait allowance across this page, starting at the first
        // candidate that needs Lustre reads.
        let tick_deadline = *dispatch
            .preflight_until
            .get_or_init(|| Instant::now() + Rev8TickTask::INTERVAL);
        if Instant::now() >= tick_deadline {
            return Ok(ControlFlow::Break("migration_preflight_tick_budget"));
        }
        let key = PreflightKey {
            run: dispatch.run.id,
            revision: dispatch.run.revision,
            fid,
            branch: candidate.branch,
            mount: dispatch.config.mount_path.clone(),
            catalog_version: (candidate.size, candidate.mtime, candidate.ctime),
            catalog_atime: dispatch.policy.uses_last_access.then_some(candidate.atime),
        };
        let request = PreflightRequest {
            key: key.clone(),
            tick_deadline,
            run_deadline: deadline,
        };
        let backend = Arc::clone(&self.runtime.as_ref().expect("dispatch requires a runtime").hsm);
        let scope = dispatch.policy.directory_scope();
        let (source, deadline) = match self
            .pool_preflights
            .read(
                &self.pool,
                filesystem,
                request,
                observe_pool_source(backend, key, invocation.clone(), scope, deadline),
            )
            .await?
        {
            PreflightResult::Observed(Ok(source), deadline) => (source, deadline),
            PreflightResult::Observed(Err(reason), _) => {
                return self
                    .record_deferred(dispatch, candidate, invocation, &reason)
                    .await
                    .map(ControlFlow::Continue);
            }
            PreflightResult::Waiting(wait @ PreflightWait::Pending) => {
                return Ok(ControlFlow::Break(wait.migration_reason()));
            }
            PreflightResult::Waiting(wait @ PreflightWait::Busy) if dispatch.policy.one_shot => {
                return Ok(ControlFlow::Break(wait.migration_reason()));
            }
            PreflightResult::Waiting(wait) => {
                self.record_deferred(dispatch, candidate, invocation, wait.migration_reason())
                    .await?;
                return Ok(ControlFlow::Continue(()));
            }
        };
        let PoolSource {
            generation,
            layout: source,
            access_time,
            links,
        } = source;
        let expected_access = dispatch.policy.uses_last_access.then_some(access_time);
        if let Some(access) = expected_access
            && access.seconds != candidate.atime
        {
            EntryStore::with_pool(self.pool.clone())
                .advance_lustre_atime(filesystem, &fid, access.seconds, generation.mtime, generation.ctime)
                .await
                .map_err(|error| PolicyError::Store(error.to_string()))?;
            return self
                .record_deferred(dispatch, candidate, invocation, "source_access_time_changed")
                .await
                .map(ControlFlow::Continue);
        }
        if source.pool.as_deref() != Some(&options.source_pool) {
            return self
                .record_deferred(dispatch, candidate, invocation, "source_pool_changed")
                .await
                .map(ControlFlow::Continue);
        }
        let plan = MigrationPlan {
            source_pool: options.source_pool.clone(),
            target_pool: options.target_pool.clone(),
            stripe_count: options
                .stripe_count
                .or_else(|| u16::try_from(source.count).ok())
                .ok_or_else(|| PolicyError::Store("unsupported source stripe count".into()))?,
            stripe_size: options
                .stripe_size
                .or_else(|| u32::try_from(source.size).ok())
                .ok_or_else(|| PolicyError::Store("unsupported source stripe size".into()))?,
        };
        // A client can go offline after the periodic sampler last visited
        // this FID. Recheck before holding catalog or policy row locks.
        if let Some(profile) = dispatch.heat
            && let Some(reason) = profile.refresh_for_admission(&self.pool, fid).await?
        {
            return self
                .record_deferred(dispatch, candidate, invocation, reason)
                .await
                .map(ControlFlow::Continue);
        }
        let ownership = match MigrationOwnership::acquire(&generation.filesystem_name) {
            Ok(ownership) => ownership,
            Err(error) => {
                return self
                    .record_deferred(dispatch, candidate, invocation, &error.to_string())
                    .await
                    .map(ControlFlow::Continue);
            }
        };
        let operation = self
            .reserve_operation(dispatch, candidate, invocation, &generation, Some(&plan), &self.pool)
            .await?;
        let mut tx = self.pool.begin().await?;
        let active = current_revision(&mut tx, dispatch.run).await?;
        if !lock_permit(&mut tx, operation).await? {
            return Ok(ControlFlow::Continue(()));
        }
        let mut late_result = None;
        let result = if !active {
            Some(Err(MigrationError::Deferred("policy_revision_inactive")))
        } else if let Some(reason) = candidate_deferral(&mut tx, dispatch, candidate).await? {
            Some(Err(MigrationError::Deferred(reason)))
        } else if !verify_worker_permit(worker).await? {
            Some(Err(MigrationError::Deferred(
                "permit_store_requires_synchronous_commit",
            )))
        } else if !dispatch.admission.window_open(Utc::now()) {
            Some(Err(MigrationError::Deferred("admission_window_closed")))
        } else if Instant::now() < deadline {
            let expected = file_version(&generation);
            let mount = dispatch.config.mount_path.clone();
            let window_open = dispatch.admission.window_check();
            let (swap_gate, _owner) = SubmissionGate::owned(move || {
                if window_open() {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(SubmissionDeferral::WindowClosed)
                }
            });
            // Moving this guard into the closure retains ownership for the
            // real blocking operation, even after the run budget expires.
            let scoped = links.is_some();
            let mut migration = Box::pin(lustre_call(move || {
                let limits = MigrationLimits {
                    deadline,
                    expected_access,
                    check_links: |source: &std::fs::File| match &links {
                        Some(links) => links.recheck(&mount, &fid, source, deadline),
                        None => Ok(()),
                    },
                    before_swap: || swap_gate.check().is_continue(),
                };
                let result = LustreApi.migrate_reserved_pool(&mount, &fid, &plan, expected, limits, &ownership);
                // Recovery must wait until a possible late deferral has
                // been journaled, including after the syscall returns.
                (result, ownership)
            }));
            match tokio::time::timeout_at(deadline.into(), &mut migration).await {
                Ok(Ok((result, _ownership))) => Some(result),
                Ok(Err(error)) => Some(Err(MigrationError::Uncertain(error.to_string()))),
                Err(_) => {
                    if scoped {
                        late_result = Some(migration);
                    }
                    None
                }
            }
        } else {
            Some(Err(MigrationError::Deferred("migration_deadline_elapsed")))
        };

        // Release the policy, journal and catalog locks before updating the
        // catalog. The committed intent remains pending through this gap.
        tx.commit().await?;
        let (state, reason, evidence) = match result {
            Some(Ok(layout)) => {
                let entries = EntryStore::with_pool(self.pool.clone());
                settle_layout(
                    &entries,
                    &dispatch.policy.filesystem,
                    fid,
                    layout,
                    file_version(&generation),
                )
                .await
            }
            Some(Err(MigrationError::Deferred(reason))) => ("deferred", Some(reason.to_owned()), None),
            Some(Err(error @ MigrationError::BeforeSwap(_))) => ("failed", Some(error.to_string()), None),
            Some(Err(error @ MigrationError::Uncertain(_))) => ("unknown", Some(error.to_string()), None),
            None => ("unknown", Some(MIGRATION_DEADLINE_PENDING.into()), None),
        };
        finish_migration(self, operation, "submitting", state, reason, evidence).await?;
        if let Some(pending) = late_result {
            let pool = self.pool.clone();
            tokio::spawn(async move {
                if let Ok((Err(MigrationError::Deferred(reason)), _ownership)) = pending.await
                    && let Err(error) = record_late_deferral(&pool, operation, reason).await
                {
                    tracing::warn!(operation, %error, reason, "could not record the late pool migration deferral");
                }
            });
        }
        Ok(ControlFlow::Continue(()))
    }

    pub(super) async fn start_pending_pool_recoveries(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        let mut query = QueryBuilder::new(format!(
            "SELECT {OPERATION_FIELDS} FROM rev8_operations o JOIN rev8_runs r ON r.id = o.run_id
             WHERE o.pending_migration_fs IS NOT NULL"
        ));
        let mut slots = {
            let recoveries = self.pool_recoveries.lock().await;
            let slots = BATCH_SIZE.saturating_sub(recoveries.len());
            if slots == 0 {
                return Ok(());
            }
            if !recoveries.is_empty() {
                exclude_recovery_ids(&mut query, recoveries.keys().copied());
            }
            slots
        };
        query
            .push(" ORDER BY o.checked_at, o.id LIMIT ")
            .push_bind(BATCH_SIZE as u32);
        let pending: Vec<Rev8Operation> = query.build_query_as().fetch_all(&self.pool).await?;
        for operation in pending {
            let filesystem =
                FileSystemId::new(&operation.filesystem).map_err(|error| PolicyError::Store(error.to_string()))?;
            if self.start_pool_recovery(entries, filesystem, operation).await? {
                slots -= 1;
                if slots == 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn start_pool_recovery(
        &self, entries: &EntryStore, filesystem: FileSystemId, operation: Rev8Operation,
    ) -> Result<bool, PolicyError> {
        let fid = decode_fid(&operation.fid)?;
        let plan = operation
            .migration_plan
            .as_ref()
            .ok_or_else(|| PolicyError::Store("pending migration has no layout plan".into()))?
            .0
            .clone();
        let generation = &operation
            .generation
            .as_ref()
            .ok_or_else(|| PolicyError::Store("pending migration has no source generation".into()))?
            .0;
        let expected = file_version(generation);
        let Some(config) = entries
            .get_filesystem(&filesystem)
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?
            .filter(|config| config.backend == BackendKind::Lustre)
        else {
            finish_migration(
                self,
                operation.id,
                &operation.state,
                "unknown",
                Some("filesystem_unavailable".into()),
                None,
            )
            .await?;
            return Ok(false);
        };
        let ownership = match MigrationOwnership::acquire(&generation.filesystem_name) {
            Ok(ownership) => ownership,
            Err(error) => {
                tracing::debug!(%filesystem, %fid, operation_id = operation.id, %error, "pool recovery waiting for migration ownership");
                // Let later journal entries through while the previous worker
                // still owns this Lustre filesystem.
                sqlx::query("UPDATE rev8_operations SET checked_at = UTC_TIMESTAMP(6) WHERE id = ?")
                    .bind(operation.id)
                    .execute(&self.pool)
                    .await?;
                return Ok(false);
            }
        };
        let mount = config.mount_path;
        let observation_mount = mount.clone();
        // Recovery only observes placement. A stalled Lustre call must not
        // hold Tokio's runtime open during daemon shutdown.
        let task = thread::Builder::new()
            .name("rev8-pool-recovery".into())
            .spawn(move || {
                let result =
                    LustreApi.reconcile_reserved_pool_migration(&observation_mount, &fid, &plan, expected, &ownership);
                (result, ownership)
            })
            .map_err(|error| PolicyError::Store(format!("start pool recovery worker: {error}")))?;
        self.pool_recoveries.lock().await.insert(
            operation.id,
            PoolRecovery {
                operation,
                filesystem,
                fid,
                mount,
                expected,
                task,
            },
        );
        Ok(true)
    }

    pub(super) async fn finish_pool_recoveries(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        // Read-only Lustre calls may block. Only join finished observations
        // while holding the catalog-wide dispatch permit.
        let finished: Vec<_> = self
            .pool_recoveries
            .lock()
            .await
            .iter()
            .filter(|(_, recovery)| recovery.task.is_finished())
            .take(BATCH_SIZE)
            .map(|(&id, _)| id)
            .collect();
        for id in finished {
            let recovery = self.pool_recoveries.lock().await.remove(&id);
            if let Some(recovery) = recovery {
                self.finish_pool_recovery(entries, recovery).await?;
            }
        }
        Ok(())
    }

    async fn finish_pool_recovery(&self, entries: &EntryStore, recovery: PoolRecovery) -> Result<(), PolicyError> {
        let PoolRecovery {
            operation,
            filesystem,
            fid,
            mount,
            expected,
            task,
        } = recovery;
        // Another daemon may have settled the operation while this worker
        // was observing. Validate the journal and mount before publishing.
        let current: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM rev8_operations
             WHERE id = ? AND pending_migration_fs IS NOT NULL AND state = ? AND checked_at <=> ?)",
        )
        .bind(operation.id)
        .bind(&operation.state)
        .bind(operation.checked_at)
        .fetch_one(&self.pool)
        .await?;
        if !current
            || !entries
                .get_filesystem(&filesystem)
                .await
                .map_err(|error| PolicyError::Store(error.to_string()))?
                .is_some_and(|config| config.backend == BackendKind::Lustre && config.mount_path == mount)
        {
            return Ok(());
        }
        let (result, _ownership) = match task.join() {
            Ok((result, ownership)) => (result, Some(ownership)),
            Err(_) => (
                Err(MigrationError::Uncertain("migration_recovery_worker_panicked".into())),
                None,
            ),
        };
        let (state, reason, evidence) = match result {
            Ok(MigrationPlacement::Target(layout)) => settle_layout(entries, &filesystem, fid, *layout, expected).await,
            Ok(MigrationPlacement::SourceUnchanged) => {
                ("deferred", Some("swap_not_observed_in_source_pool".into()), None)
            }
            Ok(MigrationPlacement::SourceChanged) => ("deferred", Some("source_generation_changed".into()), None),
            Ok(MigrationPlacement::Missing) => ("deferred", Some("fid_missing".into()), None),
            Ok(MigrationPlacement::Unresolved) => ("unknown", Some("migration_layout_unresolved".into()), None),
            Err(error) => ("unknown", Some(error.to_string()), None),
        };
        finish_migration(self, operation.id, &operation.state, state, reason, evidence).await
    }
}

async fn observe_pool_source(
    backend: Arc<dyn NativeHsm>, key: PreflightKey, invocation: Invocation, scope: Option<Arc<DirectoryScope>>,
    deadline: Instant,
) -> PoolPreflight {
    let PreflightKey {
        fid,
        mount,
        catalog_version,
        ..
    } = key;
    if Instant::now() >= deadline {
        return Err(PreflightWait::TimedOut.migration_reason().into());
    }
    let observation = backend.observe_for_migration(&mount, fid).await.map_err(Cow::Owned)?;
    let FidObservation::Present(observation) = observation else {
        return Err("fid_missing".into());
    };
    let links = if let Some(scope) = scope {
        let scope_mount = mount.clone();
        let links = lustre_call(move || scope.admit(&scope_mount, &fid, deadline))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        Some(links)
    } else {
        None
    };
    let generation = observation.generation;
    if (generation.size, generation.mtime, generation.ctime) != catalog_version {
        return Err("catalog_generation_changed".into());
    }
    invocation.precondition(&observation.state)?;
    if Instant::now() >= deadline {
        return Err(PreflightWait::TimedOut.migration_reason().into());
    }
    let layout = lustre_call(move || LustreApi.migration_source_layout(&mount, &fid))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    Ok(PoolSource {
        generation,
        layout,
        access_time: observation.access_time,
        links,
    })
}

async fn candidate_deferral(
    tx: &mut Transaction<'_, MySql>, dispatch: &Dispatch<'_>, candidate: &Candidate,
) -> Result<Option<&'static str>, PolicyError> {
    let policy = dispatch.policy;
    let entry = sqlx::query_scalar::<_, i64>(
        "SELECT CAST(1 AS SIGNED) FROM scoped_entries
         WHERE filesystem_id = ? AND object_kind = 0 AND object_id = ?
         AND size = ? AND mtime = ? AND ctime = ? FOR UPDATE",
    )
    .bind(policy.filesystem.as_str())
    .bind(&candidate.fid)
    .bind(candidate.size)
    .bind(candidate.mtime)
    .bind(candidate.ctime)
    .fetch_optional(&mut **tx)
    .await?;
    if entry.is_none() {
        return Ok(Some("catalog_candidate_changed"));
    }
    let mut query = QueryBuilder::new("SELECT CAST(((");
    policy.push_scope(&mut query, None);
    query.push(") AND ((");
    push_expression(&mut query, &policy.decision, &policy.decision_params);
    query.push(") = ").push_bind(candidate.branch).push(")) AS SIGNED)");
    if let Some(profile) = dispatch.heat {
        query.push(", read_heat.heat_status FROM scoped_entries AS entries ");
        heat::push_scores(&mut query, profile, Utc::now(), Some(&candidate.fid));
    } else {
        query.push(", NULL FROM scoped_entries AS entries ");
    }
    query
        .push(" WHERE entries.filesystem_id = ")
        .push_bind(policy.filesystem.as_str())
        .push(" AND entries.object_kind = 0 AND entries.object_id = ")
        .push_bind(candidate.fid.clone())
        .push(" FOR UPDATE");
    let (matched, heat_status) = query
        .build_query_as::<(Option<i64>, Option<String>)>()
        .fetch_one(&mut **tx)
        .await?;
    Ok(match matched {
        Some(1) => None,
        None if dispatch.heat.is_some() => Some(heat::deferral_reason(heat_status.as_deref())),
        _ => Some("catalog_candidate_changed"),
    })
}

fn file_version(generation: &Generation) -> FileVersion {
    FileVersion {
        size: generation.size,
        mtime: generation.mtime,
        mtime_nanos: generation.mtime_nanos,
        ctime: generation.ctime,
        ctime_nanos: generation.ctime_nanos,
    }
}

fn migration_deadline(dispatch: &Dispatch<'_>) -> Option<Instant> {
    let scopes = dispatch
        .admission
        .safety
        .iter()
        .filter(|scope| safety_matches(scope, dispatch.policy.filesystem.as_str(), ActionKind::PoolMigrate));
    let cap = dispatch
        .policy
        .budget
        .duration_ms
        .into_iter()
        .chain(scopes.filter_map(|scope| BudgetLimits::from_safety(scope).duration_ms))
        .min()?;
    let remaining = cap.checked_sub(elapsed_ms(dispatch.run))?;
    Instant::now().checked_add(Duration::from_millis(remaining))
}

async fn settle_layout(
    entries: &EntryStore, filesystem: &FileSystemId, fid: LuFid, placement: VerifiedPlacement, source: FileVersion,
) -> (&'static str, Option<String>, Option<serde_json::Value>) {
    match persist_layout(entries, filesystem, fid, placement, source).await {
        Ok(evidence) => ("completed", None, Some(evidence)),
        Err(error) => {
            tracing::warn!(%filesystem, %fid, %error, "migration layout catalog update pending");
            ("unknown", Some(format!("catalog_layout_pending: {error}")), None)
        }
    }
}

async fn persist_layout(
    entries: &EntryStore, filesystem: &FileSystemId, fid: LuFid, placement: VerifiedPlacement, source: FileVersion,
) -> Result<serde_json::Value, PolicyError> {
    let layout = &placement.layout;
    let evidence = json!({
        "source": "lustre_layout", "observedAtMicros": layout.observed_at, "pool": layout.pool_name,
        "ostIndices": layout.stripe_items, "stripeCount": layout.stripe_count, "stripeSize": layout.stripe_size,
    });
    let updated = entries
        .refresh_lustre_migration(filesystem, &fid, placement, source)
        .await
        .map_err(|error| PolicyError::Store(error.to_string()))?;
    if !updated {
        let key = EntryKey::new(filesystem.clone(), ObjectId::Lustre(fid));
        let indexed = entries
            .get_scoped_entry(&key)
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?;
        if indexed.is_none_or(|entry| {
            evidence["pool"] != json!(entry.pool_name)
                || evidence["ostIndices"] != json!(entry.stripe_items)
                || evidence["stripeCount"] != json!(entry.stripe_count)
                || evidence["stripeSize"] != json!(entry.stripe_size)
        }) {
            return Err(PolicyError::Store(format!(
                "catalog has no matching layout for {filesystem}/{fid}"
            )));
        }
    }
    Ok(evidence)
}

async fn finish_migration(
    store: &PolicyStore, operation: u64, previous: &str, state: &str, reason: Option<String>,
    evidence: Option<serde_json::Value>,
) -> Result<(), PolicyError> {
    sqlx::query(
        "UPDATE rev8_operations SET state = ?, attempted = attempted OR ? <> 'deferred', reason = ?,
         completion_evidence = ?, checked_at = UTC_TIMESTAMP(6),
         finished_at = CASE WHEN ? IN ('completed','failed','deferred') THEN UTC_TIMESTAMP(6) ELSE NULL END
         WHERE id = ? AND state = ?",
    )
    .bind(state)
    .bind(state)
    .bind(reason.as_deref().map(short_reason))
    .bind(evidence.map(Json))
    .bind(state)
    .bind(operation)
    .bind(previous)
    .execute(&store.pool)
    .await?;
    Ok(())
}

async fn record_late_deferral(pool: &sqlx::MySqlPool, operation: u64, reason: &'static str) -> Result<(), sqlx::Error> {
    // A typed deferral from the original worker proves the swap was never
    // attempted. Preserve that reason even if layout recovery ran first.
    sqlx::query(
        "UPDATE rev8_operations
         SET state = 'deferred', attempted = FALSE, reason = ?, checked_at = UTC_TIMESTAMP(6),
             finished_at = UTC_TIMESTAMP(6)
         WHERE id = ? AND (
            state = 'unknown'
            OR (state = 'deferred' AND reason IN ('swap_not_observed_in_source_pool', 'source_generation_changed', 'fid_missing'))
         )",
    )
    .bind(reason)
    .bind(operation)
    .execute(pool)
    .await?;
    Ok(())
}
