use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use lustre_api::{
    LustreApiError,
    migration::valid_pool_name,
    pools::{MountIdentity, OstCapacity, OstPool, OstSpaceReader, PoolMount},
};
use rbh_entry_store::FileSystemConfig;
use tokio::{
    sync::{Mutex, Semaphore, oneshot},
    time::{Instant, timeout_at},
};

use crate::{
    capacity::{self, Capacity, CapacityHistory, CapacityProgress, CapacityStatus, ProbeFailure},
    observation::{CollectionStatus, FilesystemInventory, MountResource, MountStatus, PoolResource, ResourceSnapshot},
    unix_now,
};

impl From<OstPool> for PoolResource {
    fn from(pool: OstPool) -> Self {
        let (ost_indices, eligible, reason, capacity) = match pool.members {
            Ok(members) => {
                let reason = if !valid_pool_name(&pool.name) {
                    Some("unsupported_pool_name")
                } else if members.is_empty() {
                    Some("pool_has_no_osts")
                } else {
                    None
                };
                (
                    Some(members),
                    reason.is_none(),
                    reason.map(str::to_owned),
                    Capacity::pending(),
                )
            }
            Err(error) => {
                let reason = lustre_error_message(&error);
                let capacity = Capacity::unavailable(CapacityStatus::Error, reason.clone());
                (None, false, Some(reason), capacity)
            }
        };
        Self {
            name: pool.name,
            ost_indices,
            eligible,
            capacity,
            reason,
        }
    }
}

const PROBE_LIMIT: Duration = Duration::from_secs(5);
const OST_CONCURRENCY: usize = 8;

pub(crate) struct FilesystemProbe {
    inventory_slot: Arc<Mutex<()>>,
    capacity_slot: Arc<Mutex<()>>,
    mdt_slot: Arc<Mutex<()>>,
    ost_slots: Mutex<BTreeMap<u32, Weak<Mutex<()>>>>,
    ost_concurrency: Arc<Semaphore>,
}

impl Default for FilesystemProbe {
    fn default() -> Self {
        Self {
            inventory_slot: Arc::default(),
            capacity_slot: Arc::default(),
            mdt_slot: Arc::default(),
            ost_slots: Mutex::default(),
            ost_concurrency: Arc::new(Semaphore::new(OST_CONCURRENCY)),
        }
    }
}

pub(crate) struct ResourceCollection {
    snapshot: ResourceSnapshot,
    mount_id: Option<MountIdentity>,
    targets: Option<BTreeSet<u32>>,
}

impl ResourceCollection {
    pub(crate) fn snapshot(&self, history: &mut CapacityHistory) -> ResourceSnapshot {
        let mut result = self.snapshot.clone();
        history.apply(
            self.mount_id.clone(),
            self.targets.clone(),
            &mut result.inventory.capacity,
            result.inventory.pools.as_deref_mut(),
        );
        result
    }
}

impl FilesystemProbe {
    pub(crate) async fn observe(&self, config: Arc<FileSystemConfig>) -> ResourceCollection {
        let probe_config = config.clone();
        let (mut inventory, mounted) = match run_probe(self.inventory_slot.clone(), "rbh-lustre-inventory", move || {
            probe_filesystem(&probe_config)
        })
        .await
        {
            Ok(result) => result,
            Err(reason) => (
                FilesystemInventory::unavailable(MountStatus::Unknown, reason.code().to_owned()),
                None,
            ),
        };
        let observed_at = unix_now();
        let mut mount_identity = None;
        let mut observed_targets = None;
        if let Some((mount, mount_id)) = mounted {
            mount_identity = mount_id;
            let indices = inventory
                .pools
                .iter()
                .flatten()
                .filter_map(|pool| pool.ost_indices.as_ref())
                .flatten()
                .copied()
                .collect();
            let mount = Arc::new(mount);
            let mdt_mount = mount.clone();
            let (measurements, mdts) = tokio::join!(
                self.measure_capacity(mount, indices),
                run_probe(self.mdt_slot.clone(), "rbh-lustre-mdt", move || mdt_mount
                    .active_mdt_names()),
            );
            let (measurements, probe_error) = measurements;
            let (capacity, targets) = capacity::summarize(
                measurements,
                inventory.pools.as_deref_mut().unwrap_or_default(),
                probe_error.as_ref(),
                observed_at,
            );
            inventory.capacity = capacity;
            inventory.ost_indices = targets.clone();
            inventory.mdt_names = mdts.ok().and_then(Result::ok);
            observed_targets = targets;
            if matches!(inventory.collection_status, CollectionStatus::Collected)
                && (!inventory.capacity.is_collected()
                    || inventory
                        .pools
                        .iter()
                        .flatten()
                        .any(|pool| !pool.capacity.is_collected()))
            {
                inventory.collection_status = CollectionStatus::Partial;
            }
        }
        ResourceCollection {
            snapshot: ResourceSnapshot {
                id: config.id.clone(),
                mount_path: config.mount_path.display().to_string(),
                observed_at: Some(observed_at),
                inventory,
            },
            mount_id: mount_identity,
            targets: observed_targets,
        }
    }
    async fn measure_capacity(
        &self, mount: Arc<PoolMount>, mut indices: BTreeSet<u32>,
    ) -> (Vec<CapacityProgress>, Option<ProbeFailure>) {
        let deadline = Instant::now() + PROBE_LIMIT;
        let setup = run_probe(self.capacity_slot.clone(), "rbh-lustre-targets", move || {
            (mount.ost_targets(), mount.ost_space_reader())
        })
        .await;
        let (targets, reader) = match setup {
            Ok(result) => result,
            Err(error) => return (Vec::new(), Some(error)),
        };
        if let Ok(targets) = &targets {
            indices.extend(targets.known.iter().chain(&targets.unverified).copied());
        }
        let mut measured = vec![CapacityProgress::Targets(
            targets.map_err(|error| lustre_error_message(&error)),
        )];
        let reader = match reader {
            Ok(reader) => Arc::new(reader),
            Err(error) => return (measured, Some(ProbeFailure::Lustre(lustre_error_message(&error)))),
        };
        self.ost_slots
            .lock()
            .await
            .retain(|_, reference| reference.strong_count() > 0);
        let operations = stream::iter(indices)
            .map(|index| {
                let reader = reader.clone();
                async move { CapacityProgress::Ost(index, self.read_ost(reader, index).await) }
            })
            .buffer_unordered(OST_CONCURRENCY);
        tokio::pin!(operations);
        loop {
            match timeout_at(deadline, operations.next()).await {
                Ok(Some(result)) => measured.push(result),
                Ok(None) => return (measured, None),
                Err(_) => return (measured, Some(ProbeFailure::TimedOut)),
            }
        }
    }

    async fn read_ost(&self, reader: Arc<OstSpaceReader>, index: u32) -> Result<OstCapacity, String> {
        let slot = {
            let mut slots = self.ost_slots.lock().await;
            let reference = slots.entry(index).or_default();
            reference.upgrade().unwrap_or_else(|| {
                let slot = Arc::new(Mutex::new(()));
                *reference = Arc::downgrade(&slot);
                slot
            })
        };
        let target = slot
            .try_lock_owned()
            .map_err(|_| "上一次 OST 查询仍在运行".to_owned())?;
        let concurrent = self
            .ost_concurrency
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ProbeFailure::Failed.code().to_owned())?;
        blocking_probe("rbh-lustre-ost", move || {
            let _permits = (target, concurrent);
            reader.usage(index)
        })
        .await
        .map_err(|error| error.code().to_owned())?
        .map_err(|error| lustre_error_message(&error))
    }
}

async fn run_probe<T: Send + 'static>(
    slot: Arc<Mutex<()>>, task: &'static str, probe: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ProbeFailure> {
    tokio::time::timeout(PROBE_LIMIT, async move {
        let permit = slot.lock_owned().await;
        blocking_probe(task, move || {
            let _permit = permit;
            probe()
        })
        .await
    })
    .await
    .map_err(|_| ProbeFailure::TimedOut)?
}

async fn blocking_probe<T: Send + 'static>(
    task: &'static str, probe: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ProbeFailure> {
    let (send, receive) = oneshot::channel();
    // A kernel ioctl can outlive the request. The thread retains its permits
    // and readers until the call returns without blocking Tokio shutdown.
    std::thread::Builder::new()
        .name(task.into())
        .spawn(move || {
            let _ = send.send(probe());
        })
        .map_err(|error| {
            tracing::warn!(task, %error, "could not start Lustre probe");
            ProbeFailure::Failed
        })?;
    receive.await.map_err(|_| ProbeFailure::Failed)
}

fn probe_filesystem(config: &FileSystemConfig) -> (FilesystemInventory, Option<(PoolMount, Option<MountIdentity>)>) {
    let mounted = match PoolMount::open(&config.mount_path) {
        Ok(mounted) => mounted,
        Err(LustreApiError::NotALustreMount(_)) => {
            return (
                FilesystemInventory::unavailable(MountStatus::Unmounted, "not_a_lustre_mount".to_owned()),
                None,
            );
        }
        Err(error) => {
            return (
                FilesystemInventory::unavailable(MountStatus::Unknown, lustre_error_message(&error)),
                None,
            );
        }
    };
    let mount = MountResource {
        source: mounted.source().to_owned(),
        read_only: mounted.read_only(),
    };
    let mount_id = mounted.identity();
    let mut pools: Vec<_> = match mounted.ost_pools() {
        Ok(pools) => pools.into_iter().map(PoolResource::from).collect(),
        Err(error) => {
            return (
                FilesystemInventory {
                    mount_status: MountStatus::Mounted,
                    mount: Some(mount),
                    capacity: Capacity::pending(),
                    collection_status: CollectionStatus::Error,
                    pools: None,
                    ost_indices: None,
                    mdt_names: None,
                    tiering_available: false,
                    reason: Some(lustre_error_message(&error)),
                },
                Some((mounted, mount_id)),
            );
        }
    };
    pools.sort_by(|a, b| a.name.cmp(&b.name));
    let collection_status = if pools.iter().any(|pool| pool.ost_indices.is_none()) {
        CollectionStatus::Partial
    } else {
        CollectionStatus::Collected
    };
    let reason = if mounted.read_only() {
        Some("read_only_mount")
    } else if !config.supports_lustre_hsm() {
        Some("lustre_hsm_capability_required")
    } else if !config.supports_lustre_pool_migration() {
        Some("stripe_and_ost_capabilities_required")
    } else if pools.iter().filter(|pool| pool.eligible).take(2).count() < 2 {
        Some("two_eligible_pools_required")
    } else {
        None
    };
    (
        FilesystemInventory {
            mount_status: MountStatus::Mounted,
            mount: Some(mount),
            capacity: Capacity::pending(),
            collection_status,
            pools: Some(pools),
            ost_indices: None,
            mdt_names: None,
            tiering_available: reason.is_none(),
            reason: reason.map(str::to_owned),
        },
        Some((mounted, mount_id)),
    )
}

fn lustre_error_message(error: &LustreApiError) -> String {
    match error {
        LustreApiError::Io(error) => error.to_string(),
        error => error.to_string(),
    }
}
