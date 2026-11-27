use std::{borrow::Cow, collections::BTreeMap, future::Future, path::PathBuf, sync::Arc, time::Instant};

use lustre_api::{LuFid, StripeInfo};
use rbh_entry_store::FileSystemId;
use sqlx::MySqlPool;
use tokio::{sync::Mutex, task::JoinHandle};

use super::{BATCH_SIZE, Generation, PolicyError};

#[derive(Default, Clone)]
pub(in crate::store) struct PoolPreflights(Arc<Mutex<BTreeMap<FileSystemId, PendingRead>>>);

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PreflightKey {
    pub run: u64,
    pub revision: u64,
    pub fid: LuFid,
    pub mount: PathBuf,
    pub catalog_version: (u64, i64, i64),
}

pub(super) struct PreflightRequest {
    pub key: PreflightKey,
    pub tick_deadline: Instant,
    pub run_deadline: Instant,
}

pub(super) type PreflightObservation = Result<(Generation, StripeInfo), Cow<'static, str>>;

struct PendingRead {
    key: PreflightKey,
    deadline: Instant,
    task: JoinHandle<PreflightObservation>,
}

pub(super) enum PreflightResult {
    Observed(PreflightObservation, Instant),
    Waiting(PreflightWait),
}

pub(super) enum PreflightWait {
    Busy,
    Pending,
    TimedOut,
    WorkerStopped,
}

impl PreflightWait {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Self::Busy => "migration_preflight_busy",
            Self::Pending => "migration_preflight_pending",
            Self::TimedOut => "migration_preflight_timeout",
            Self::WorkerStopped => "migration_preflight_worker_stopped",
        }
    }
}

impl PoolPreflights {
    #[tracing::instrument(skip_all, fields(%filesystem, run_id = request.key.run, fid = %request.key.fid))]
    pub(super) async fn read<F>(
        &self, pool: &MySqlPool, filesystem: &FileSystemId, request: PreflightRequest, observe: F,
    ) -> Result<PreflightResult, PolicyError>
    where
        F: Future<Output = PreflightObservation> + Send + 'static,
    {
        use PreflightResult::*;
        use PreflightWait::*;

        // Only the Rev.8 worker accesses the registry. The shared database
        // worker permit already serializes ticks within this store.
        let mut active = self.0.lock().await;
        if let Some(previous) = active.get(filesystem)
            && previous.key != request.key
        {
            if !previous.task.is_finished() {
                return Ok(Waiting(if previous.key.run == request.key.run {
                    Pending
                } else {
                    Busy
                }));
            }
            // Keep a completed read for its queued owner while the run is
            // still eligible. Expired, cancelled or superseded work can be replaced.
            if previous.key.run != request.key.run
                && Instant::now() < previous.deadline
                && owner_is_active(pool, &previous.key).await?
            {
                return Ok(Waiting(Busy));
            }
            active.remove(filesystem);
        }
        if !active.contains_key(filesystem) {
            if active.values().filter(|job| !job.task.is_finished()).count() >= BATCH_SIZE {
                return Ok(Waiting(Busy));
            }
            if Instant::now() >= request.run_deadline {
                return Ok(Waiting(TimedOut));
            }
            if Instant::now() >= request.tick_deadline {
                return Ok(Waiting(Pending));
            }
            active.insert(
                filesystem.clone(),
                PendingRead {
                    key: request.key,
                    deadline: request.run_deadline,
                    task: tokio::spawn(observe),
                },
            );
        }
        let job = active.get_mut(filesystem).expect("preflight job exists");
        job.deadline = job.deadline.min(request.run_deadline);
        let deadline = job.deadline;
        let wait_until = deadline.min(request.tick_deadline);
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(wait_until.into()) => {
                // Keep both the slot and the observation across ticks. An
                // expired read still occupies its slot until it returns.
                if Instant::now() >= deadline {
                    if job.task.is_finished() {
                        active.remove(filesystem);
                    }
                    Ok(Waiting(TimedOut))
                } else {
                    Ok(Waiting(Pending))
                }
            }
            result = &mut job.task => {
                active.remove(filesystem);
                if Instant::now() >= deadline {
                    return Ok(Waiting(TimedOut));
                }
                Ok(match result {
                    Ok(observation) => Observed(observation, deadline),
                    Err(error) => {
                        tracing::warn!(%error, "pool migration preflight worker stopped");
                        Waiting(WorkerStopped)
                    }
                })
            }
        }
    }
}

async fn owner_is_active(pool: &MySqlPool, key: &PreflightKey) -> Result<bool, PolicyError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM rev8_runs r JOIN policies p ON p.id = r.policy_id AND p.revision = r.revision
         WHERE r.id = ? AND r.revision = ? AND r.state IN ('queued','running') AND p.enabled)",
    )
    .bind(key.run)
    .bind(key.revision)
    .fetch_one(pool)
    .await?)
}
