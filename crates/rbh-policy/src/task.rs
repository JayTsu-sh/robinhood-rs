//! Scheduler wakeup for Rev.8 policies.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use scheduler_rs::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::mysql::MySqlDatabaseError;
use tokio::sync::mpsc;

use crate::PolicyError;

/// Shared runtime context injected once at daemon startup.
pub struct PolicyRuntime {
    pub policy_store: crate::PolicyStore,
    pub entry_store: rbh_entry_store::store::EntryStore,
    pub worker_failure: mpsc::Sender<PolicyError>,
}

static RUNTIME: OnceLock<Arc<PolicyRuntime>> = OnceLock::new();

pub fn init_runtime(rt: Arc<PolicyRuntime>) {
    if RUNTIME.set(rt).is_err() {
        panic!("init_runtime called more than once — this is a startup bug");
    }
}

fn runtime() -> &'static Arc<PolicyRuntime> {
    RUNTIME
        .get()
        .expect("PolicyRuntime not initialized — call init_runtime() at startup")
}

/// Wake the durable Rev.8 run queue and reconcile pending native operations.
#[derive(Clone, Serialize, Deserialize)]
pub struct Rev8TickTask;

impl Rev8TickTask {
    pub const INTERVAL: Duration = Duration::from_secs(1);
}

#[async_trait]
impl Task for Rev8TickTask {
    const TYPE_NAME: &'static str = "rbh.rev8_tick";

    async fn run(&self, ctx: &TaskContext) -> TaskResult {
        if ctx.cancellation_token.is_cancelled() {
            return Ok(());
        }
        let rt = runtime();
        let Err(error) = rt.policy_store.process_rev8(&rt.entry_store).await else {
            return Ok(());
        };
        let message = error.to_string();
        if scheduler_store_unavailable(&error)
            && let Err(error) = rt.worker_failure.try_send(error)
        {
            tracing::debug!(%error, "Rev.8 worker failure channel no longer accepting errors");
        }
        Err(SchedulerError::ExecutionError(message))
    }
}

fn scheduler_store_unavailable(error: &PolicyError) -> bool {
    match error {
        PolicyError::WorkerLockMissing | PolicyError::Db(sqlx::Error::PoolClosed) => true,
        PolicyError::Db(sqlx::Error::Database(error)) => {
            // These errors require restoring the schema or database access.
            error
                .try_downcast_ref::<MySqlDatabaseError>()
                .is_some_and(|error| matches!(error.number(), 1044 | 1045 | 1054 | 1142 | 1146))
        }
        _ => false,
    }
}
