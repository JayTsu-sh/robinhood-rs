use std::time::{Duration, Instant};

use chrono::Utc;
use lustre_api::{
    LuFid, LustreApi,
    migration::{
        FileVersion, MigrationError, MigrationOwnership, MigrationPlacement, MigrationPlan, VerifiedPlacement,
    },
};
use rbh_entry_store::{EntryKey, EntryStore, FileSystemId, ObjectId};
use serde_json::json;
use sqlx::{MySql, QueryBuilder, Transaction, types::Json};

use super::{
    Candidate, Dispatch, Generation, Invocation, PolicyError, PolicyStore, Rev8Operation, current_revision, decode_fid,
    elapsed_ms, lock_permit, short_reason, verify_worker_permit,
};
use crate::rev8::{ActionKind, BudgetLimits, heat, push_expression, resources::safety_matches};

impl PolicyStore {
    pub(super) async fn admit_pool_migration(
        &self, dispatch: &Dispatch<'_>, candidate: &Candidate, invocation: &Invocation, generation: &Generation,
        worker: &mut Transaction<'_, MySql>,
    ) -> Result<(), PolicyError> {
        let options = invocation.migration.as_ref().expect("pool migration invocation");
        if generation.size > options.max_file_bytes {
            return self
                .record_deferred(dispatch, candidate, invocation, "migration_max_file_bytes")
                .await;
        }
        let mount = dispatch.config.mount_path.clone();
        let fid = decode_fid(&candidate.fid)?;
        let source = {
            let mount = mount.clone();
            tokio::task::spawn_blocking(move || LustreApi.migration_source_layout(&mount, &fid))
                .await
                .map_err(|error| PolicyError::Store(error.to_string()))?
        };
        let source = match source {
            Ok(source) if source.pool.as_deref() == Some(&options.source_pool) => source,
            Ok(_) => {
                return self
                    .record_deferred(dispatch, candidate, invocation, "source_pool_changed")
                    .await;
            }
            Err(error) => {
                return self
                    .record_deferred(dispatch, candidate, invocation, &error.to_string())
                    .await;
            }
        };
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
            return self.record_deferred(dispatch, candidate, invocation, reason).await;
        }
        let ownership = match MigrationOwnership::acquire(&generation.filesystem_name) {
            Ok(ownership) => ownership,
            Err(error) => {
                return self
                    .record_deferred(dispatch, candidate, invocation, &error.to_string())
                    .await;
            }
        };
        let operation = self
            .reserve_operation(dispatch, candidate, invocation, generation, Some(&plan))
            .await?;
        let mut tx = self.pool.begin().await?;
        let active = current_revision(&mut tx, dispatch.run).await?;
        if !lock_permit(&mut tx, operation).await? {
            return Ok(());
        }
        let result = if !active {
            Err(MigrationError::Deferred("policy_revision_inactive"))
        } else if let Some(reason) = candidate_deferral(&mut tx, dispatch, candidate).await? {
            Err(MigrationError::Deferred(reason))
        } else if !verify_worker_permit(worker).await? {
            Err(MigrationError::Deferred("permit_store_requires_synchronous_commit"))
        } else if !dispatch.admission.window_open(Utc::now()) {
            Err(MigrationError::Deferred("admission_window_closed"))
        } else if let Some(deadline) = migration_deadline(dispatch) {
            let expected = file_version(generation);
            // Moving this guard into the closure retains ownership for the
            // real blocking operation, even if the scheduler loses its waiter.
            match tokio::task::spawn_blocking(move || {
                LustreApi.migrate_reserved_pool(&mount, &fid, &plan, expected, deadline, ownership)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => Err(MigrationError::Uncertain(error.to_string())),
            }
        } else {
            Err(MigrationError::Deferred("migration_deadline_elapsed"))
        };

        // Release the policy, journal and catalog locks before updating the
        // catalog. The committed intent remains pending through this gap.
        tx.commit().await?;
        let (state, reason, evidence) = match result {
            Ok(layout) => {
                let entries = EntryStore::with_pool(self.pool.clone());
                settle_layout(
                    &entries,
                    &dispatch.policy.filesystem,
                    fid,
                    layout,
                    file_version(generation),
                )
                .await
            }
            Err(MigrationError::Deferred(reason)) => ("deferred", Some(reason.to_owned()), None),
            Err(error @ MigrationError::BeforeSwap(_)) => ("failed", Some(error.to_string()), None),
            Err(error @ MigrationError::Uncertain(_)) => ("unknown", Some(error.to_string()), None),
        };
        finish_migration(self, operation, "submitting", state, reason, evidence).await
    }

    pub(super) async fn reconcile_pool_operation(
        &self, entries: &EntryStore, filesystem: &FileSystemId, operation: &Rev8Operation,
    ) -> Result<(), PolicyError> {
        let fid = decode_fid(&operation.fid)?;
        let plan = operation
            .migration_plan
            .as_ref()
            .ok_or_else(|| PolicyError::Store("pending migration has no layout plan".into()))?
            .0
            .clone();
        let generation = operation
            .generation
            .as_ref()
            .ok_or_else(|| PolicyError::Store("pending migration has no source generation".into()))?
            .0
            .clone();
        let expected = file_version(&generation);
        let result = if let Some(config) = entries
            .get_filesystem(filesystem)
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?
            && config.backend == rbh_entry_store::BackendKind::Lustre
        {
            tokio::task::spawn_blocking(move || {
                LustreApi.reconcile_pool_migration(
                    &config.mount_path,
                    &fid,
                    &plan,
                    expected,
                    &generation.filesystem_name,
                )
            })
            .await
            .map_err(|error| PolicyError::Store(error.to_string()))?
        } else {
            Err(MigrationError::Deferred("filesystem_unavailable"))
        };
        let (state, reason, evidence) = match result {
            Ok(MigrationPlacement::Target(layout)) => settle_layout(entries, filesystem, fid, *layout, expected).await,
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
