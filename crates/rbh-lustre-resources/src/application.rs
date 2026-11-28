use std::{sync::Arc, time::Duration};

use futures_util::future::join_all;
use rbh_entry_store::{BackendKind, FileSystemConfig, FileSystemId};
use sqlx::MySqlPool;
use tokio::sync::{Mutex, watch};

use crate::{
    capacity::CapacityHistory,
    collector::FilesystemProbe,
    observation::ResourceSnapshot,
    repository::{Observation, SnapshotRepository},
};

type RefreshResult = watch::Receiver<Option<Result<(), String>>>;

/// Daemon registration, read access to saved observations, and collection on demand.
#[derive(Clone)]
pub struct ResourceObservations {
    snapshots: SnapshotRepository,
    probes: Arc<[Arc<RegisteredResource>]>,
    boot_id: Option<Arc<str>>,
    stopping: watch::Sender<bool>,
}

struct RegisteredResource {
    config: Arc<FileSystemConfig>,
    probe: FilesystemProbe,
    refresh: Mutex<Option<RefreshResult>>,
}

impl ResourceObservations {
    pub fn new(pool: MySqlPool, observer_id: String, mut filesystems: Vec<FileSystemConfig>) -> Self {
        filesystems.retain(|fs| fs.backend == BackendKind::Lustre);
        filesystems.sort_by(|a, b| a.id.cmp(&b.id));
        let probes = filesystems
            .into_iter()
            .map(|config| {
                Arc::new(RegisteredResource {
                    config: Arc::new(config),
                    probe: FilesystemProbe::default(),
                    refresh: Mutex::new(None),
                })
            })
            .collect();
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|value| Arc::from(value.trim()));
        Self {
            snapshots: SnapshotRepository::new(pool, observer_id),
            probes,
            boot_id,
            stopping: watch::channel(false).0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.probes.is_empty()
    }

    pub fn stop(&self) {
        self.stopping.send_replace(true);
    }

    fn probe(&self, id: &FileSystemId) -> Option<&Arc<RegisteredResource>> {
        self.probes.iter().find(|probe| &probe.config.id == id)
    }

    pub fn contains(&self, id: &FileSystemId) -> bool {
        self.probe(id).is_some()
    }

    #[tracing::instrument(skip(self))]
    pub async fn get(&self, id: &FileSystemId) -> Result<Option<ResourceSnapshot>, sqlx::Error> {
        let Some(probe) = self.probe(id) else {
            return Ok(None);
        };
        let saved = self
            .snapshots
            .load(id)
            .await?
            .map(|(_, saved)| saved)
            .filter(|saved| saved.config == *probe.config);
        Ok(Some(saved.map_or_else(
            || ResourceSnapshot::pending(&probe.config),
            |saved| saved.resource,
        )))
    }

    #[tracing::instrument(skip(self))]
    pub async fn list(&self) -> Result<Vec<ResourceSnapshot>, sqlx::Error> {
        if self.is_empty() {
            return Ok(Vec::new());
        }
        let ids = self.probes.iter().map(|probe| &probe.config.id);
        let mut saved = self.snapshots.list(ids).await?;
        Ok(self
            .probes
            .iter()
            .map(|probe| {
                saved
                    .remove(&probe.config.id)
                    .filter(|saved| saved.config == *probe.config)
                    .map_or_else(|| ResourceSnapshot::pending(&probe.config), |saved| saved.resource)
            })
            .collect())
    }

    /// Scheduled and manual refreshes join a collection already in progress for the same filesystem.
    #[tracing::instrument(skip(self))]
    pub async fn refresh(&self, id: Option<&FileSystemId>) -> Result<(), String> {
        if let Some(id) = id {
            let probe = self
                .probe(id)
                .ok_or_else(|| format!("unknown Lustre filesystem {id}"))?;
            self.refresh_one(probe).await
        } else {
            join_all(self.probes.iter().map(|probe| self.refresh_one(probe)))
                .await
                .into_iter()
                .collect()
        }
    }

    async fn refresh_one(&self, probe: &Arc<RegisteredResource>) -> Result<(), String> {
        let mut result = {
            let mut in_progress = probe.refresh.lock().await;
            if let Some(pending) = in_progress
                .as_ref()
                .filter(|result| result.borrow().is_none() && result.has_changed().is_ok())
            {
                pending.clone()
            } else {
                let (send, receive) = watch::channel(None);
                let resources = self.clone();
                let probe = probe.clone();
                let mut stopping = self.stopping.subscribe();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = stopping.wait_for(|stopping| *stopping) => Err("资源采集正在停止".to_owned()),
                        result = resources.observe_and_save(&probe) => result,
                    };
                    send.send_replace(Some(result));
                });
                *in_progress = Some(receive.clone());
                receive
            }
        };
        result
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "资源采集任务已退出".to_owned())?
            .as_ref()
            .expect("wait_for checked for a result")
            .clone()
    }

    async fn observe_and_save(&self, probe: &RegisteredResource) -> Result<(), String> {
        let config = &probe.config;
        let collection = probe.probe.observe(config.clone()).await;
        // Retry the database write against the latest complete samples. The
        // Lustre calls have already finished and need not be repeated.
        tokio::time::timeout(Duration::from_secs(8), async {
            for _ in 0..3 {
                let stored = self
                    .snapshots
                    .load(&config.id)
                    .await
                    .map_err(|error| error.to_string())?;
                let revision = stored.as_ref().map(|(revision, _)| *revision);
                let mut history = stored
                    .map(|(_, saved)| saved)
                    .filter(|saved| {
                        saved.config == **config
                            && self.boot_id.is_some()
                            && saved.boot_id.as_deref() == self.boot_id.as_deref()
                    })
                    .map_or_else(CapacityHistory::default, |saved| saved.history);
                let resource = collection.snapshot(&mut history);
                let observed_at = resource.observed_at.ok_or_else(|| "缺少资源观测时间".to_owned())?;
                let observation = Observation {
                    config: (**config).clone(),
                    resource,
                    boot_id: self.boot_id.as_deref().map(str::to_owned),
                    history,
                };
                if self
                    .snapshots
                    .save(observation, observed_at, revision)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    return Ok(());
                }
            }
            Err("资源快照发生并发更新，请重试".to_owned())
        })
        .await
        .map_err(|_| "保存资源快照超时".to_owned())?
    }
}
