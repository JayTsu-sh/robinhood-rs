use std::{sync::OnceLock, time::Duration};

use rbh_lustre_resources::ResourceObservations;
use scheduler_rs::{prelude::*, trigger::IntervalTrigger};
use serde::{Deserialize, Serialize};

static RESOURCES: OnceLock<ResourceObservations> = OnceLock::new();

#[derive(Clone, Serialize, Deserialize)]
struct CollectLustreResources;

#[async_trait]
impl Task for CollectLustreResources {
    const TYPE_NAME: &'static str = "rbh.collect_lustre_resources";

    async fn run(&self, ctx: &TaskContext) -> TaskResult {
        let resources = RESOURCES
            .get()
            .expect("console collections require a registered runtime");
        tokio::select! {
            _ = ctx.cancellation_token.cancelled() => Ok(()),
            result = resources.refresh(None) => result.map_err(SchedulerError::ExecutionError),
        }
    }
}

#[tracing::instrument(skip(scheduler, resources))]
pub(crate) async fn register(scheduler: &Scheduler, resources: ResourceObservations, interval: Duration) -> TaskResult {
    RESOURCES
        .set(resources)
        .map_err(|_| SchedulerError::ExecutionError("console collection already registered".into()))?;
    scheduler.register::<CollectLustreResources>().await;
    let config = ScheduleConfig {
        misfire_policy: MisfirePolicy::Coalesce,
        max_instances: 1,
        ..Default::default()
    };
    let trigger = IntervalTrigger::every(interval).starting_at(chrono::Utc::now() + chrono::Duration::seconds(1));
    scheduler
        .add_raw(
            CollectLustreResources::TYPE_NAME.to_owned(),
            serde_json::Value::Null,
            Box::new(trigger),
            config,
            Some("rbh.lustre_resources".into()),
        )
        .await?;
    Ok(())
}
