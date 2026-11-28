use std::{collections::BTreeMap, time::Duration};

use rbh_entry_store::FileSystemId;
use rbh_policy::{PolicyStore, policy::heat::HeatSampler};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub(crate) async fn supervise(
    store: PolicyStore, intervals: BTreeMap<FileSystemId, Duration>, cancel: CancellationToken,
) {
    let mut workers = BTreeMap::<FileSystemId, CancellationToken>::new();
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        let active = match store.active_read_heat_filesystems().await {
            Ok(active) => active,
            Err(error) => {
                tracing::warn!(%error, "could not refresh Lustre read heat sampling scope");
                continue;
            }
        };
        workers.retain(|filesystem, worker| {
            if active.contains(filesystem) {
                return true;
            }
            worker.cancel();
            tracing::info!(%filesystem, "Lustre read heat sampler stopped");
            false
        });
        for filesystem in active {
            let interval = intervals[&filesystem];
            workers.entry(filesystem.clone()).or_insert_with(|| {
                let worker = cancel.child_token();
                tracing::info!(%filesystem, "Lustre read heat sampler started");
                tokio::spawn(sample(store.clone(), filesystem, interval, worker.clone()));
                worker
            });
        }
    }
}

async fn sample(store: PolicyStore, filesystem: FileSystemId, interval: Duration, cancel: CancellationToken) {
    let mut sampler = HeatSampler::default();
    let mut ticks = tokio::time::interval(interval);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        if let Err(error) = store.sample_read_heat_batch(&filesystem, &mut sampler).await {
            tracing::warn!(%filesystem, %error, "Lustre read heat batch failed");
        }
    }
}
