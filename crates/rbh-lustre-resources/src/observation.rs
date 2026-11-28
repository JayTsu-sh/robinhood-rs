use std::collections::BTreeSet;

use crate::capacity::{Capacity, CapacityStatus};
use rbh_entry_store::{FileSystemConfig, FileSystemId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MountStatus {
    Mounted,
    Unmounted,
    Unknown,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CollectionStatus {
    Pending,
    Collected,
    Partial,
    Unavailable,
    Error,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub(crate) id: FileSystemId,
    pub(crate) mount_path: String,
    pub(crate) observed_at: Option<u64>,
    #[serde(flatten)]
    pub(crate) inventory: FilesystemInventory,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilesystemInventory {
    pub(crate) mount_status: MountStatus,
    pub(crate) collection_status: CollectionStatus,
    pub(crate) mount: Option<MountResource>,
    pub(crate) capacity: Capacity,
    pub(crate) pools: Option<Vec<PoolResource>>,
    pub(crate) tiering_available: bool,
    pub(crate) reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MountResource {
    pub(crate) source: String,
    pub(crate) read_only: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PoolResource {
    pub(crate) name: String,
    pub(crate) ost_indices: Option<BTreeSet<u32>>,
    pub(crate) eligible: bool,
    pub(crate) reason: Option<String>,
    pub(crate) capacity: Capacity,
}

impl ResourceSnapshot {
    pub(crate) fn pending(config: &FileSystemConfig) -> Self {
        Self {
            id: config.id.clone(),
            mount_path: config.mount_path.display().to_string(),
            observed_at: None,
            inventory: FilesystemInventory {
                mount_status: MountStatus::Unknown,
                collection_status: CollectionStatus::Pending,
                mount: None,
                capacity: Capacity::pending(),
                pools: None,
                tiering_available: false,
                reason: Some("collection_pending".to_owned()),
            },
        }
    }
}

impl FilesystemInventory {
    pub(crate) fn unavailable(mount_status: MountStatus, reason: String) -> Self {
        let (collection_status, capacity_status) = match mount_status {
            MountStatus::Unmounted => (CollectionStatus::Unavailable, CapacityStatus::Missing),
            _ => (CollectionStatus::Error, CapacityStatus::Error),
        };
        Self {
            mount_status,
            collection_status,
            mount: None,
            capacity: Capacity::unavailable(capacity_status, reason.clone()),
            pools: None,
            tiering_available: false,
            reason: Some(reason),
        }
    }
}
