//! Scheduler wakeup for Rev.8 policies.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use async_trait::async_trait;
use scheduler_rs::prelude::*;
use serde::{Deserialize, Serialize};

/// Shared runtime context injected once at daemon startup.
pub struct PolicyRuntime {
    pub policy_store: crate::PolicyStore,
    pub entry_store: rbh_entry_store::store::EntryStore,
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
        rt.policy_store
            .process_rev8(&rt.entry_store)
            .await
            .map_err(|error| SchedulerError::ExecutionError(error.to_string()))
    }
}
