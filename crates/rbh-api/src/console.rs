//! Same-origin console assets and inventory of this daemon's registered Lustre resources.

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, header},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::future::join_all;
use lustre_api::{
    LustreApiError,
    migration::valid_pool_name,
    pools::{OstPool, PoolMount},
};
use rbh_entry_store::{BackendKind, FileSystemConfig, FileSystemId};
use serde::Serialize;
use tokio::sync::{Mutex, oneshot};
use tower_http::{services::ServeDir, trace::TraceLayer};

pub(crate) fn router(mut filesystems: Vec<FileSystemConfig>, assets_dir: PathBuf) -> Router {
    filesystems.retain(|fs| fs.backend == BackendKind::Lustre);
    filesystems.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let probes: Arc<[_]> = filesystems
        .into_iter()
        .map(|config| FilesystemProbe {
            config: Arc::new(config),
            slot: Arc::new(Mutex::new(())),
        })
        .collect();
    let resources = Router::new()
        .route("/api/filesystems", get(list_filesystems))
        .with_state(probes);
    let assets = Router::new()
        .nest_service("/console", ServeDir::new(assets_dir))
        .layer(middleware::map_response(|mut response: Response| async {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            response
        }));
    resources.merge(assets).layer(TraceLayer::new_for_http())
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum MountStatus {
    Mounted,
    Unmounted,
    Unknown,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum CollectionStatus {
    Collected,
    Partial,
    Unavailable,
    Error,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesystemResource {
    id: FileSystemId,
    mount_path: String,
    observed_at: u64,
    #[serde(flatten)]
    inventory: FilesystemInventory,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FilesystemInventory {
    mount_status: MountStatus,
    collection_status: CollectionStatus,
    pools: Option<Vec<PoolResource>>,
    tiering_available: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PoolResource {
    name: String,
    ost_indices: Option<BTreeSet<u32>>,
    eligible: bool,
    reason: Option<String>,
}

impl From<OstPool> for PoolResource {
    fn from(pool: OstPool) -> Self {
        let (ost_indices, eligible, reason) = match pool.members {
            Ok(members) => {
                let reason = if !valid_pool_name(&pool.name) {
                    Some("unsupported_pool_name")
                } else if members.is_empty() {
                    Some("pool_has_no_osts")
                } else {
                    None
                };
                (Some(members), reason.is_none(), reason.map(str::to_owned))
            }
            Err(error) => (None, false, Some(lustre_error_message(error))),
        };
        Self {
            name: pool.name,
            ost_indices,
            eligible,
            reason,
        }
    }
}

struct FilesystemProbe {
    config: Arc<FileSystemConfig>,
    slot: Arc<Mutex<()>>,
}

impl FilesystemProbe {
    async fn observe(&self) -> FilesystemResource {
        let config = self.config.clone();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let permit = self.slot.clone().lock_owned().await;
            let (send, receive) = oneshot::channel();
            // A stalled kernel call must neither start a second probe for this
            // filesystem nor keep the daemon's Tokio runtime from shutting down.
            std::thread::Builder::new()
                .name("rbh-lustre-inventory".into())
                .spawn(move || {
                    let _permit = permit;
                    let _ = send.send(probe_filesystem(&config));
                })
                .map_err(|error| error.to_string())?;
            receive.await.map_err(|error| error.to_string())
        })
        .await;
        let inventory = match result {
            Ok(Ok(inventory)) => inventory,
            Ok(Err(error)) => {
                tracing::warn!(filesystem = %self.config.id, %error, "Lustre inventory probe failed");
                FilesystemInventory::unavailable(MountStatus::Unknown, "probe_failed".to_owned())
            }
            Err(_) => FilesystemInventory::unavailable(MountStatus::Unknown, "probe_timed_out".to_owned()),
        };
        FilesystemResource {
            id: self.config.id.clone(),
            mount_path: self.config.mount_path.display().to_string(),
            observed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            inventory,
        }
    }
}

#[tracing::instrument(skip_all)]
async fn list_filesystems(State(probes): State<Arc<[FilesystemProbe]>>) -> Response {
    let rows = join_all(probes.iter().map(FilesystemProbe::observe)).await;
    ([(header::CACHE_CONTROL, "no-store")], Json(rows)).into_response()
}

impl FilesystemInventory {
    fn unavailable(mount_status: MountStatus, reason: String) -> Self {
        let collection_status = match mount_status {
            MountStatus::Unmounted => CollectionStatus::Unavailable,
            _ => CollectionStatus::Error,
        };
        Self {
            mount_status,
            collection_status,
            pools: None,
            tiering_available: false,
            reason: Some(reason),
        }
    }
}

fn probe_filesystem(config: &FileSystemConfig) -> FilesystemInventory {
    let mounted = match PoolMount::open(&config.mount_path) {
        Ok(mounted) => mounted,
        Err(LustreApiError::NotALustreMount(_)) => {
            return FilesystemInventory::unavailable(MountStatus::Unmounted, "not_a_lustre_mount".to_owned());
        }
        Err(error) => {
            return FilesystemInventory::unavailable(MountStatus::Unknown, lustre_error_message(error));
        }
    };
    let mut pools: Vec<_> = match mounted.ost_pools() {
        Ok(pools) => pools.into_iter().map(PoolResource::from).collect(),
        Err(error) => return FilesystemInventory::unavailable(MountStatus::Mounted, lustre_error_message(error)),
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
    FilesystemInventory {
        mount_status: MountStatus::Mounted,
        collection_status,
        pools: Some(pools),
        tiering_available: reason.is_none(),
        reason: reason.map(str::to_owned),
    }
}

fn lustre_error_message(error: LustreApiError) -> String {
    match error {
        LustreApiError::Io(error) => error.to_string(),
        error => error.to_string(),
    }
}
