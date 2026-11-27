use std::{
    collections::BTreeMap,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::Utc;
use lustre_api::HsmState;
use rbh_entry_store::{EntryStore, FileSystemId};
use serde_json::{Value, json};
use sqlx::{MySql, QueryBuilder, types::Json};
use tokio::task::JoinHandle;

use super::{BATCH_SIZE, OPERATION_FIELDS, PolicyStore, Rev8Operation, decode_fid, exclude_recovery_ids};
use crate::{
    PolicyError,
    rev8::{
        self, ActionKind, Invocation,
        execution::{FidObservation, Generation, LustreHsm, NativeHsm},
        invalid,
    },
};

// Reserve capacity for other scopes even if every observation on one mount stalls.
const NATIVE_RECOVERIES_PER_FILESYSTEM: u32 = 8;

pub(in crate::store) struct NativeRecovery {
    operation: Rev8Operation,
    filesystem: FileSystemId,
    mount: PathBuf,
    task: JoinHandle<Option<Settlement>>,
}

enum Settlement {
    Completed(Json<Value>),
    TargetNotReached(Json<Value>),
    FidMissing(Json<Value>),
}

impl PolicyStore {
    pub(super) async fn start_pending_native_recoveries(&self, entries: &EntryStore) -> Result<(), PolicyError> {
        let mut query = QueryBuilder::new(
            "WITH recoverable AS (
                SELECT o.id, ROW_NUMBER() OVER (PARTITION BY o.filesystem ORDER BY o.checked_at, o.id) AS scope_position
                FROM rev8_operations o WHERE o.pending_fid IS NOT NULL AND o.pending_migration_fs IS NULL",
        );
        let (slots, counts) = {
            let recoveries = self.native_recoveries.lock().await;
            let slots = BATCH_SIZE.saturating_sub(recoveries.len());
            if slots == 0 {
                return Ok(());
            }
            let mut counts = BTreeMap::new();
            for recovery in recoveries.values() {
                *counts.entry(recovery.filesystem.clone()).or_default() += 1;
            }
            if !recoveries.is_empty() {
                exclude_recovery_ids(&mut query, recoveries.keys().copied());
            }
            (slots, counts)
        };
        query.push(format!(
            ") SELECT {OPERATION_FIELDS} FROM recoverable q JOIN rev8_operations o ON o.id = q.id
             JOIN rev8_runs r ON r.id = o.run_id WHERE q.scope_position <= "
        ));
        limit_native_recoveries(&mut query, counts);
        query
            .push(" ORDER BY o.checked_at, o.id LIMIT ")
            .push_bind(slots as u32);
        let pending: Vec<Rev8Operation> = query.build_query_as().fetch_all(&self.pool).await?;
        for operation in pending {
            let filesystem =
                FileSystemId::new(&operation.filesystem).map_err(|error| PolicyError::Store(error.to_string()))?;
            self.start_native_recovery(entries, filesystem, operation).await?;
        }
        Ok(())
    }

    pub(super) async fn awaiting_native_predecessor(
        &self, filesystem: &str, fid: &[u8], action: ActionKind,
    ) -> Result<bool, PolicyError> {
        // These actions can become eligible as soon as the preceding request
        // is confirmed. A poll cursor or local observer is not proof of that outcome.
        let predecessor = match action {
            ActionKind::Release => ActionKind::Archive,
            ActionKind::Restore => ActionKind::Release,
            _ => return Ok(false),
        };
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM rev8_operations WHERE filesystem = ? AND pending_fid = ? AND action = ?)",
        )
        .bind(filesystem)
        .bind(fid)
        .bind(predecessor.name())
        .fetch_one(&self.pool)
        .await?)
    }

    async fn start_native_recovery(
        &self, entries: &EntryStore, filesystem: FileSystemId, operation: Rev8Operation,
    ) -> Result<(), PolicyError> {
        let action = ActionKind::from_name(&operation.action).ok_or_else(|| invalid("unknown persisted HSM action"))?;
        let invocation = Invocation {
            kind: action,
            archive_id: operation.archive_id,
            requires_human: false,
            migration: None,
        };
        let fid = decode_fid(&operation.fid)?;
        let config = match rev8::validate_filesystem(entries, &filesystem).await {
            Ok(config) => config,
            Err(PolicyError::InvalidPolicy(_)) => {
                self.settle_native_recovery(&operation, None, None).await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let backend = self.runtime.as_ref().map_or_else(
            || Arc::new(LustreHsm) as Arc<dyn NativeHsm>,
            |runtime| Arc::clone(&runtime.hsm),
        );
        let mount = config.mount_path;
        let observation_mount = mount.clone();
        let operation_id = operation.id;
        let generation = operation.generation.as_ref().map(|value| value.0.clone());
        let accepted = operation.state == "submitted";
        let mut recoveries = self.native_recoveries.lock().await;
        if recoveries.len() >= BATCH_SIZE
            || recoveries.contains_key(&operation.id)
            || recoveries
                .values()
                .filter(|recovery| recovery.filesystem == filesystem)
                .count()
                >= NATIVE_RECOVERIES_PER_FILESYSTEM as usize
        {
            return Ok(());
        }
        // Register the task before the next cancellation point.
        let task = tokio::spawn(async move {
            let observe = || async {
                backend
                    .observe_in_background(&observation_mount, fid)
                    .await
                    .inspect_err(|error| {
                        tracing::debug!(%fid, operation_id, %error, "HSM observation unavailable");
                    })
                    .ok()
            };
            let mut observation = observe().await;
            let confirmed_idle = if accepted
                && let (Some(generation), Some(FidObservation::Present(observed))) = (&generation, &observation)
                && generation.mount_source_id.is_some()
                && generation.same_content(&observed.generation)
                && observed.state.coordinator_idle()
                && !invocation.completed(&observed.state)
            {
                // The first flags may predate completion because the current
                // action is queried last. Reread flags after observing idle.
                observation = observe().await;
                true
            } else {
                false
            };
            native_settlement(&invocation, generation.as_ref(), observation, accepted, confirmed_idle)
        });
        recoveries.insert(
            operation.id,
            NativeRecovery {
                operation,
                filesystem,
                mount,
                task,
            },
        );
        Ok(())
    }

    pub(super) async fn finish_native_recoveries(&self) -> Result<(), PolicyError> {
        // Harvest only completed observations under the catalog worker permit.
        let finished: Vec<_> = self
            .native_recoveries
            .lock()
            .await
            .iter()
            .filter(|(_, recovery)| recovery.task.is_finished())
            .map(|(&id, _)| id)
            .collect();
        for id in finished {
            let recovery = self.native_recoveries.lock().await.remove(&id);
            let Some(recovery) = recovery else {
                continue;
            };
            let settlement = recovery.task.await.unwrap_or_else(|error| {
                tracing::warn!(operation_id = id, %error, "native HSM recovery worker failed");
                None
            });
            self.settle_native_recovery(&recovery.operation, settlement, Some(&recovery.mount))
                .await?;
        }
        Ok(())
    }

    async fn settle_native_recovery(
        &self, operation: &Rev8Operation, settlement: Option<Settlement>, mount: Option<&Path>,
    ) -> Result<(), PolicyError> {
        let terminal = settlement.is_some();
        let completes_interrupted_submission =
            operation.state == "submitting" && matches!(&settlement, Some(Settlement::Completed(_)));
        let (state, reason, evidence) = match settlement {
            Some(Settlement::Completed(evidence)) => ("completed", None, Some(evidence)),
            Some(Settlement::TargetNotReached(evidence)) => {
                ("failed", Some("hsm_target_state_not_reached"), Some(evidence))
            }
            Some(Settlement::FidMissing(evidence)) => ("failed", Some("fid_missing"), Some(evidence)),
            // An interrupted ioctl can still be in progress even if the coordinator is idle.
            None if operation.state == "submitting" => ("unknown", Some("interrupted_submission"), None),
            None => (operation.state.as_str(), operation.reason.as_deref(), None),
        };
        // Terminal evidence may close a still-pending operation even if
        // another worker published an inconclusive check in the meantime.
        let mount = mount.map(|path| path.as_os_str().as_bytes());
        sqlx::query(
            "UPDATE rev8_operations o LEFT JOIN filesystems f ON f.id = o.filesystem
             SET o.state = ?, o.reason = ?, o.checked_at = UTC_TIMESTAMP(6),
                 o.finished_at = CASE WHEN ? IN ('completed', 'failed') THEN UTC_TIMESTAMP(6) ELSE NULL END,
                 o.completion_evidence = ?, o.attempted = TRUE
             WHERE o.id = ? AND o.pending_fid IS NOT NULL
             AND (o.state = ? OR (? AND o.state = 'unknown' AND o.reason = 'interrupted_submission'))
             AND (? OR o.checked_at <=> ?)
             AND (? IS NULL OR (f.backend_kind = 'lustre' AND f.mount_path = ?
                 AND JSON_UNQUOTE(JSON_EXTRACT(f.capabilities, '$.hsm')) = 'true'))",
        )
        .bind(state)
        .bind(reason)
        .bind(state)
        .bind(evidence)
        .bind(operation.id)
        .bind(&operation.state)
        .bind(completes_interrupted_submission)
        .bind(terminal)
        .bind(operation.checked_at)
        .bind(mount)
        .bind(mount)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn limit_native_recoveries(query: &mut QueryBuilder<'_, MySql>, counts: BTreeMap<FileSystemId, u32>) {
    if counts.is_empty() {
        query.push_bind(NATIVE_RECOVERIES_PER_FILESYSTEM);
    } else {
        query.push("CASE o.filesystem");
        for (filesystem, count) in counts {
            query
                .push(" WHEN ")
                .push_bind(filesystem.to_string())
                .push(" THEN ")
                .push_bind(NATIVE_RECOVERIES_PER_FILESYSTEM.saturating_sub(count));
        }
        query
            .push(" ELSE ")
            .push_bind(NATIVE_RECOVERIES_PER_FILESYSTEM)
            .push(" END");
    }
}

fn native_settlement(
    invocation: &Invocation, generation: Option<&Generation>, observation: Option<FidObservation>, accepted: bool,
    confirmed_idle: bool,
) -> Option<Settlement> {
    let generation = generation?;
    match observation? {
        FidObservation::Present(observed) if generation.same_content(&observed.generation) => {
            let completed = invocation.completed(&observed.state);
            let source = if completed {
                "lustre_hsm_state"
            } else if confirmed_idle && observed.state.coordinator_idle() && consistent_hsm_flags(observed.state.states)
            {
                "lustre_hsm_current_action"
            } else {
                return None;
            };
            let evidence = Json(json!({
                "source": source, "observedAt": Utc::now(),
                "state": observed.state, "generation": observed.generation,
            }));
            Some(if completed {
                Settlement::Completed(evidence)
            } else {
                Settlement::TargetNotReached(evidence)
            })
        }
        FidObservation::Missing { mount_source_id }
            if accepted && generation.mount_source_id.as_deref() == Some(&mount_source_id) =>
        {
            Some(Settlement::FidMissing(Json(json!({
                "source": "lustre_fid_lookup", "observedAt": Utc::now(), "mountSourceId": mount_source_id,
            }))))
        }
        _ => None,
    }
}

fn consistent_hsm_flags(flags: HsmState) -> bool {
    use HsmState as S;
    HsmState::from_bits(flags.bits()).is_some()
        && (!flags.contains(S::DIRTY) || flags.contains(S::EXISTS) && !flags.contains(S::RELEASED))
        && (!flags.contains(S::RELEASED) || flags.contains(S::ARCHIVED))
        && (!flags.contains(S::LOST) || flags.contains(S::ARCHIVED))
}
