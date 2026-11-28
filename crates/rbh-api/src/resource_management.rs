//! Console-managed Lustre client registrations and mount lifecycle.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Extension, Json, Router,
    extract::{Path as ApiPath, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig, FileSystemId, error::StoreError};
use rbh_lustre_resources::ResourceObservations;
use serde::{Deserialize, Serialize};
use sqlx::types::Json as Stored;
use tokio::{
    process::Command,
    sync::{Mutex, MutexGuard},
};

use crate::{
    AppState, PolicyWriters,
    handlers::{ApiError, StartScanRequest, start_scan},
    state::{ScanRecord, ScanState},
};

const CONTAINER_MANAGED_ROOT: &str = "/mnt";
const LEGACY_CONTAINER_MANAGED_ROOT: &str = "/mnt/rbh-lustre";

/// Marks requests received through the dedicated console gateway socket.
#[derive(Clone, Copy)]
pub struct ConsoleGateway;

#[derive(Clone)]
pub struct ResourceAdministration {
    app: AppState,
    observations: ResourceObservations,
    observer: Arc<str>,
    managed_root: Option<PathBuf>,
    deployment_ids: Arc<BTreeSet<FileSystemId>>,
    changes: Arc<Mutex<()>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceAdministrationError {
    #[error("cannot inspect RBH_LUSTRE_MANAGED_ROOT at {path:?}")]
    ManagedRootIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("RBH_LUSTRE_MANAGED_ROOT must be a canonical, private writable directory")]
    InvalidManagedRoot,
    #[error("could not load console Lustre registrations")]
    ReadRegistrations(#[from] sqlx::Error),
    #[error("could not restore registration for filesystem {id}")]
    RestoreRegistration {
        id: FileSystemId,
        #[source]
        source: StoreError,
    },
}

fn validate_managed_root(root: &Path) -> Result<(), ResourceAdministrationError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let (canonical, metadata) = root
        .canonicalize()
        .and_then(|path| path.metadata().map(|metadata| (path, metadata)))
        .map_err(|source| ResourceAdministrationError::ManagedRootIo {
            path: root.to_path_buf(),
            source,
        })?;
    if canonical != root
        || root == Path::new("/")
        || !metadata.is_dir()
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.uid() != nix::unistd::geteuid().as_raw()
    {
        return Err(ResourceAdministrationError::InvalidManagedRoot);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Owner {
    Deployment,
    External,
    Daemon,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Tier {
    Hot,
    Warm,
    Cold,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    id: FileSystemId,
    name: String,
    owner: Owner,
    path: PathBuf,
    source: String,
    options: Vec<String>,
    auto_mount: bool,
    desired_mounted: bool,
    #[serde(default)]
    unmount_pending: bool,
    removed: bool,
    tiers: BTreeMap<String, Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_mount_error: Option<String>,
}

impl Registration {
    fn configuration(&self) -> FileSystemConfig {
        FileSystemConfig {
            id: self.id.clone(),
            mount_path: self.path.clone(),
            backend: BackendKind::Lustre,
            capabilities: BackendCapabilities {
                changelog: false,
                namespace: true,
                purge: false,
                hsm: true,
                stripe: true,
                ost: true,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagementView {
    can_create_mount: bool,
    resources: Vec<RegistrationView>,
    available_mounts: Vec<DetectedMount>,
}

#[derive(Serialize)]
struct RegistrationView {
    #[serde(flatten)]
    registration: Registration,
    #[serde(skip_serializing_if = "Option::is_none")]
    mounted: Option<bool>,
}

#[derive(Clone, Serialize)]
struct DetectedMount {
    path: PathBuf,
    source: String,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum NewMount {
    Existing {
        path: PathBuf,
    },
    Managed {
        filesystem: String,
        mgs_nids: String,
        path: Option<PathBuf>,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        auto_mount: bool,
        #[serde(default)]
        mount_now: bool,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegisterRequest {
    id: FileSystemId,
    name: String,
    mount: NewMount,
}

#[derive(Deserialize)]
#[serde(
    tag = "action",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Change {
    Rename { name: String },
    SetTier { pool: String, tier: Option<Tier> },
    SetAutoMount { enabled: bool },
    Mount,
    Unmount,
    Remove,
}

impl ResourceAdministration {
    pub fn new(
        app: AppState, observations: ResourceObservations, observer: String, managed_root: Option<PathBuf>,
    ) -> Result<Self, ResourceAdministrationError> {
        if let Some(root) = &managed_root {
            validate_managed_root(root)?;
        }
        let deployment_ids = Arc::new(observations.registered_ids().into_iter().collect());
        Ok(Self {
            app,
            observations,
            observer: observer.into(),
            managed_root,
            deployment_ids,
            changes: Arc::default(),
        })
    }

    #[tracing::instrument(skip(self))]
    pub async fn restore(&self) -> Result<(), ResourceAdministrationError> {
        let resources = self.records().await?;
        for mut registration in resources
            .into_iter()
            .filter(|r| !r.removed && r.owner != Owner::Deployment)
        {
            if self.observations.contains(&registration.id) {
                tracing::warn!(filesystem = %registration.id, "deployment registration takes precedence");
                continue;
            }
            self.register_runtime(&registration).await.map_err(|source| {
                ResourceAdministrationError::RestoreRegistration {
                    id: registration.id.clone(),
                    source,
                }
            })?;
            if registration.owner == Owner::Daemon && registration.unmount_pending {
                if let Err(error) = self.finish_unmount(&mut registration).await {
                    tracing::warn!(filesystem = %registration.id, ?error, "could not finish pending Lustre unmount");
                }
                continue;
            }
            if registration.owner == Owner::Daemon
                && registration.auto_mount
                && registration.desired_mounted
                && let Err(error) = self.mount_and_record(&mut registration).await
            {
                tracing::warn!(filesystem = %registration.id, ?error, "could not restore Lustre mount");
            }
            if let Ok(mounts) = read_mounts()
                && mounted(&registration, &mounts)
            {
                self.start_baseline(&registration).await;
            }
        }
        Ok(())
    }

    async fn mount_and_record(&self, resource: &mut Registration) -> Result<(), ApiError> {
        let result = self.mount(resource).await;
        resource.last_mount_error = result.as_ref().err().map(|error| match error {
            ApiError::BadRequest(message) | ApiError::ServiceUnavailable(message) => message.clone(),
            _ => "无法挂载文件系统".into(),
        });
        self.save_and_notify(resource).await?;
        result
    }

    pub fn routes(self, writers: PolicyWriters) -> Router {
        Router::new()
            .route("/api/filesystems/management", get(list).post(register))
            .route("/api/filesystems/management/check", post(check))
            .route("/api/filesystems/management/{id}", post(change))
            .layer(Extension(writers))
            .with_state(self)
    }

    async fn records(&self) -> Result<Vec<Registration>, sqlx::Error> {
        Ok(sqlx::query_scalar::<_, Stored<Registration>>(
            "SELECT metadata FROM console_lustre_resources WHERE observer_id = ? ORDER BY filesystem_id",
        )
        .bind(self.observer.as_bytes())
        .fetch_all(self.app.entry_store.pool())
        .await?
        .into_iter()
        .map(|row| row.0)
        .collect())
    }

    async fn save(&self, registration: &Registration) -> Result<(), ApiError> {
        self.write_registration(registration, self.app.entry_store.pool()).await
    }

    async fn save_and_notify(&self, registration: &Registration) -> Result<(), ApiError> {
        self.save(registration).await?;
        self.observations.management_changed(Some(registration.id.clone()));
        Ok(())
    }

    async fn write_registration<'e>(
        &self, registration: &Registration, executor: impl sqlx::Executor<'e, Database = sqlx::MySql>,
    ) -> Result<(), ApiError> {
        sqlx::query(
            "INSERT INTO console_lustre_resources (observer_id, filesystem_id, metadata) VALUES (?, ?, ?)
             ON DUPLICATE KEY UPDATE metadata = VALUES(metadata)",
        )
        .bind(self.observer.as_bytes())
        .bind(registration.id.as_str())
        .bind(Stored(registration))
        .execute(executor)
        .await
        .map_err(database_error)?;
        Ok(())
    }

    async fn register_runtime(&self, registration: &Registration) -> Result<(), StoreError> {
        let config = registration.configuration();
        self.app.entry_store.register_filesystem(&config).await?;
        self.app
            .policy_store
            .set_console_mount_source(&registration.id, registration.source.clone());
        self.observations.register(config);
        self.app.policy_store.set_filesystem_configured(&registration.id, true);
        Ok(())
    }

    async fn start_baseline(&self, registration: &Registration) {
        let id = &registration.id;
        if !self
            .app
            .entry_store
            .scoped_entry_count(id)
            .await
            .is_ok_and(|count| count == 0)
            || self
                .app
                .scans
                .lock()
                .await
                .values()
                .any(|scan| scan.filesystem == *id && scan.state == ScanState::Running)
        {
            return;
        }
        if let Err(error) = start_scan(
            State(self.app.clone()),
            Json(StartScanRequest {
                filesystem: id.clone(),
                root: None,
                since_mtime: None,
                ignore_globs: Vec::new(),
                concurrency: None,
                max_depth: None,
            }),
        )
        .await
        {
            tracing::warn!(filesystem = %id, ?error, "could not start Lustre baseline scan");
        }
    }

    fn mount_path(&self, id: &FileSystemId, path: Option<PathBuf>) -> Result<PathBuf, ApiError> {
        let root = self
            .managed_root
            .as_deref()
            .ok_or_else(|| bad("部署未配置 daemon 挂载目录"))?;
        let path = path.unwrap_or_else(|| root.join(id.as_str()));
        if root == Path::new(CONTAINER_MANAGED_ROOT) && path == Path::new(LEGACY_CONTAINER_MANAGED_ROOT) {
            return Err(bad("该挂载路径保留用于恢复旧版挂载"));
        }
        validate_mount_path(root, id, &path)?;
        Ok(path)
    }

    fn uses_legacy_mount_path(&self, path: &Path) -> bool {
        self.managed_root.as_deref() == Some(Path::new(CONTAINER_MANAGED_ROOT))
            && path.parent() == Some(Path::new(LEGACY_CONTAINER_MANAGED_ROOT))
    }

    fn registered_mount_path(&self, registration: &Registration) -> Result<PathBuf, ApiError> {
        if self.uses_legacy_mount_path(&registration.path) {
            let legacy_root = Path::new(LEGACY_CONTAINER_MANAGED_ROOT);
            validate_managed_root(legacy_root).map_err(|error| {
                tracing::warn!(%error, "could not use the previous container mount root");
                bad("旧版挂载根目录不可用")
            })?;
            validate_mount_path(legacy_root, &registration.id, &registration.path)?;
            return Ok(registration.path.clone());
        }
        self.mount_path(&registration.id, Some(registration.path.clone()))
    }

    async fn mount(&self, registration: &Registration) -> Result<(), ApiError> {
        let path = self.registered_mount_path(registration)?;
        let mounts = read_mounts()?;
        if let Some(existing) = mounts.get(&path) {
            return if existing.as_deref() == Some(&registration.source) {
                Ok(())
            } else {
                Err(bad("挂载路径已占用"))
            };
        }
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.is_symlink() => {
                if std::fs::read_dir(&path).map_err(system_error)?.next().is_some() {
                    return Err(bad("挂载目录必须为空"));
                }
            }
            Ok(_) => return Err(bad("挂载路径必须是普通目录")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&path).map_err(system_error)?;
            }
            Err(error) => return Err(system_error(error)),
        }
        let options = ["nosuid", "nodev"]
            .into_iter()
            .chain(registration.options.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(",");
        let mut command = Command::new("/usr/bin/mount");
        command
            .args([
                "--no-canonicalize",
                "-t",
                "lustre",
                "-o",
                &options,
                "--",
                &registration.source,
            ])
            .arg(&path);
        execute(command).await?;
        if !mounted(registration, &read_mounts()?) {
            return Err(bad("实际挂载来源与注册配置不符"));
        }
        Ok(())
    }

    async fn finish_unmount(&self, resource: &mut Registration) -> Result<(), ApiError> {
        let path = self.registered_mount_path(resource)?;
        let mut tx = self.app.entry_store.pool().begin().await.map_err(database_error)?;
        let scans = self.require_idle(&resource.id, false, &mut tx).await?;
        let mounts = read_mounts()?;
        let is_mounted = mounted(resource, &mounts);
        if !is_mounted && mounts.contains_key(&path) {
            return Err(bad("挂载点来源已变化"));
        }
        if is_mounted {
            let previous = resource.clone();
            if !resource.unmount_pending {
                // Commit intent independently of the guard transaction so startup can finish it.
                resource.desired_mounted = false;
                resource.unmount_pending = true;
                self.save(resource).await?;
            }
            let mut command = Command::new("/usr/bin/umount");
            command.args(["--no-canonicalize", "--"]).arg(path);
            if let Err(error) = execute(command).await
                && mounted(resource, &read_mounts()?)
            {
                // A definitive failure, such as a busy mount, leaves the previous intent in place.
                // A timeout may still be completing in the kernel. Keep that intent recoverable.
                if !previous.unmount_pending && matches!(error, ApiError::BadRequest(_)) {
                    self.save(&previous).await?;
                    *resource = previous;
                }
                return Err(error);
            }
        }
        resource.desired_mounted = false;
        resource.unmount_pending = false;
        resource.last_mount_error = None;
        self.write_registration(resource, &mut *tx).await?;
        tx.commit().await.map_err(database_error)?;
        drop(scans);
        self.observations.management_changed(Some(resource.id.clone()));
        Ok(())
    }

    async fn require_idle(
        &self, id: &FileSystemId, removing: bool, tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    ) -> Result<MutexGuard<'_, HashMap<String, ScanRecord>>, ApiError> {
        // Use the same lock order as the policy worker. A new write or dispatch
        // cannot race the checks and the unmount.
        for table in ["policy_worker_lock", "policy_configuration_lock"] {
            sqlx::query(&format!("SELECT id FROM {table} WHERE id = 1 FOR UPDATE"))
                .fetch_one(&mut **tx)
                .await
                .map_err(database_error)?;
        }
        let policy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM policies WHERE name IS NOT NULL AND (enabled OR ?)
            AND JSON_VALUE(definition, '$.spec.target.filesystem') = ?)",
        )
        .bind(removing)
        .bind(id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(database_error)?;
        let operations: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM policy_operations WHERE filesystem = ? AND pending_fid IS NOT NULL)",
        )
        .bind(id.as_str())
        .fetch_one(&mut **tx)
        .await
        .map_err(database_error)?;
        let scans = self.app.scans.lock().await;
        let scanning = scans
            .values()
            .any(|scan| scan.filesystem == *id && scan.state == ScanState::Running);
        if policy || operations || scanning {
            return Err(bad("存在关联策略、正在扫描或有待核对的操作"));
        }
        Ok(scans)
    }
}

#[tracing::instrument(skip_all)]
async fn list(State(admin): State<ResourceAdministration>) -> Result<impl IntoResponse, ApiError> {
    let mut resources: Vec<_> = admin
        .records()
        .await
        .map_err(database_error)?
        .into_iter()
        .filter(|r| !r.removed && (r.owner == Owner::Deployment || !admin.deployment_ids.contains(&r.id)))
        .collect();
    for config in admin
        .app
        .entry_store
        .list_filesystems()
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
    {
        if admin.observations.contains(&config.id) {
            if let Some(resource) = resources.iter_mut().find(|resource| resource.id == config.id) {
                if resource.owner == Owner::Deployment {
                    resource.path = config.mount_path;
                }
                continue;
            }
            resources.push(Registration {
                name: config.id.to_string(),
                id: config.id,
                path: config.mount_path,
                owner: Owner::Deployment,
                source: String::new(),
                options: Vec::new(),
                auto_mount: false,
                desired_mounted: false,
                unmount_pending: false,
                removed: false,
                tiers: BTreeMap::new(),
                last_mount_error: None,
            });
        }
    }
    let mounts = read_mounts()?;
    let available_mounts = mounts
        .iter()
        .filter_map(|(path, source)| {
            if resources.iter().any(|r| &r.path == path) {
                return None;
            }
            Some(DetectedMount {
                path: path.clone(),
                source: source.as_ref()?.clone(),
            })
        })
        .collect();
    let resources = resources
        .into_iter()
        .map(|registration| {
            let mounted = match registration.owner {
                // A configured path can also refer to a subtree or alias, whose
                // status comes from the filesystem probe.
                Owner::Deployment => mounts
                    .get(&registration.path)
                    .is_some_and(Option::is_some)
                    .then_some(true),
                Owner::Daemon | Owner::External => Some(mounted(&registration, &mounts)),
            };
            RegistrationView { registration, mounted }
        })
        .collect();
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(ManagementView {
            can_create_mount: admin.managed_root.is_some(),
            resources,
            available_mounts,
        }),
    ))
}

#[tracing::instrument(skip_all)]
async fn check(
    State(admin): State<ResourceAdministration>, Extension(writers): Extension<PolicyWriters>, headers: HeaderMap,
    gateway: Option<Extension<ConsoleGateway>>, Json(request): Json<RegisterRequest>,
) -> Result<StatusCode, ApiError> {
    writers.authorize(&headers)?;
    if gateway.is_some() && matches!(request.mount, NewMount::Existing { .. }) {
        return Err(bad("外部挂载由部署管理"));
    }
    let (registration, _) = registration(&admin, request).await?;
    if registration.owner == Owner::Daemon {
        let (nids, _) = registration.source.split_once(":/").expect("validated Lustre source");
        let (host, networks) = nids.split_once('@').ok_or_else(|| bad("无法从 MGS 配置提取 NID"))?;
        let network = networks.split([',', ':']).next().unwrap_or_default();
        let nid = format!("{host}@{network}");
        let mut command = Command::new("/usr/sbin/lctl");
        command.args(["ping", &nid]);
        execute(command).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(skip_all)]
async fn register(
    State(admin): State<ResourceAdministration>, Extension(writers): Extension<PolicyWriters>, headers: HeaderMap,
    gateway: Option<Extension<ConsoleGateway>>, Json(request): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<Registration>), ApiError> {
    let actor = writers.authorize(&headers)?;
    if gateway.is_some() && matches!(request.mount, NewMount::Existing { .. }) {
        return Err(bad("外部挂载由部署管理"));
    }
    let _change = admin.changes.lock().await;
    let (mut resource, mount_now) = registration(&admin, request).await?;
    let records = admin.records().await.map_err(database_error)?;
    let retired_same_source = records
        .iter()
        .any(|r| r.removed && r.id == resource.id && r.source == resource.source && r.path == resource.path);
    let catalog = admin
        .app
        .entry_store
        .list_filesystems()
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let mounts = read_mounts()?;
    if admin.observations.contains(&resource.id)
        || records
            .iter()
            .any(|r| !r.removed && (r.path == resource.path || r.source == resource.source || r.id == resource.id))
        || catalog.iter().any(|r| {
            ((r.id == resource.id || r.mount_path == resource.path) && (!retired_same_source || r.id != resource.id))
                || (admin.observations.contains(&r.id)
                    && mounts.get(&r.mount_path).and_then(Option::as_deref) == Some(resource.source.as_str()))
        })
    {
        return Err(bad("文件系统 ID、挂载路径或集群已注册"));
    }
    admin.save(&resource).await?;
    admin
        .register_runtime(&resource)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    admin.observations.management_changed(Some(resource.id.clone()));
    if mount_now && let Err(error) = admin.mount_and_record(&mut resource).await {
        tracing::warn!(filesystem = %resource.id, ?error, "registered Lustre mount is pending");
    }
    admin.start_baseline_if_mounted(&resource).await?;
    tracing::info!(%actor, filesystem = %resource.id, "console registered Lustre filesystem");
    Ok((StatusCode::CREATED, Json(resource)))
}

impl ResourceAdministration {
    async fn start_baseline_if_mounted(&self, registration: &Registration) -> Result<(), ApiError> {
        if mounted(registration, &read_mounts()?) {
            self.start_baseline(registration).await;
        }
        self.observations
            .refresh(Some(&registration.id))
            .await
            .map_err(ApiError::ServiceUnavailable)
    }
}

#[tracing::instrument(skip_all)]
async fn change(
    State(admin): State<ResourceAdministration>, Extension(writers): Extension<PolicyWriters>,
    ApiPath(id): ApiPath<FileSystemId>, headers: HeaderMap, gateway: Option<Extension<ConsoleGateway>>,
    Json(change): Json<Change>,
) -> Result<StatusCode, ApiError> {
    let actor = writers.authorize(&headers)?;
    if gateway.is_some() && matches!(change, Change::Remove) {
        return Err(bad("请通过 daemon REST API 管理外部接入"));
    }
    let change_guard = admin.changes.clone().lock_owned().await;
    let mut resource =
        match admin.records().await.map_err(database_error)?.into_iter().find(|r| {
            r.id == id && !r.removed && (r.owner == Owner::Deployment || !admin.deployment_ids.contains(&r.id))
        }) {
            Some(resource) => resource,
            None if admin.observations.contains(&id) => {
                let config = admin
                    .app
                    .entry_store
                    .get_filesystem(&id)
                    .await
                    .map_err(|error| ApiError::Internal(error.to_string()))?
                    .ok_or_else(|| bad("未知文件系统"))?;
                Registration {
                    id: id.clone(),
                    name: id.to_string(),
                    owner: Owner::Deployment,
                    path: config.mount_path,
                    source: String::new(),
                    options: Vec::new(),
                    auto_mount: false,
                    desired_mounted: false,
                    unmount_pending: false,
                    removed: false,
                    tiers: BTreeMap::new(),
                    last_mount_error: None,
                }
            }
            None => return Err(ApiError::NotFound(format!("未知文件系统 {id}"))),
        };
    match change {
        Change::Rename { name } => {
            resource.name = valid_name(&name)?;
            admin.save_and_notify(&resource).await?;
        }
        Change::SetTier { pool, tier } => {
            if !lustre_api::pools::valid_pool_name(&pool) {
                return Err(bad("无效的 Pool 名称"));
            }
            if let Some(tier) = tier {
                resource.tiers.insert(pool, tier);
            } else {
                resource.tiers.remove(&pool);
            }
            admin.save_and_notify(&resource).await?;
        }
        Change::SetAutoMount { enabled } if resource.owner == Owner::Daemon => {
            resource.auto_mount = enabled;
            admin.save_and_notify(&resource).await?;
        }
        Change::Mount if resource.owner == Owner::Daemon => {
            resource.desired_mounted = true;
            resource.unmount_pending = false;
            admin.save(&resource).await?;
            admin.mount_and_record(&mut resource).await?;
            admin.start_baseline_if_mounted(&resource).await?;
        }
        Change::Unmount if resource.owner == Owner::Daemon => {
            tokio::spawn(async move {
                let _change = change_guard;
                admin.finish_unmount(&mut resource).await?;
                admin
                    .observations
                    .refresh(Some(&resource.id))
                    .await
                    .map_err(ApiError::ServiceUnavailable)
            })
            .await
            .map_err(|error| ApiError::Internal(format!("unmount Lustre registration: {error}")))??;
        }
        Change::Remove if resource.owner == Owner::External => {
            let removed_id = id.clone();
            // Finish the state transition even if the HTTP request goes away during commit.
            tokio::spawn(async move {
                let _change = change_guard;
                let mut tx = admin.app.entry_store.pool().begin().await.map_err(database_error)?;
                let _scans = admin.require_idle(&removed_id, true, &mut tx).await?;
                resource.removed = true;
                admin.write_registration(&resource, &mut *tx).await?;
                admin.app.policy_store.set_filesystem_configured(&removed_id, false);
                if let Err(error) = tx.commit().await {
                    admin.app.policy_store.set_filesystem_configured(&removed_id, true);
                    return Err(database_error(error));
                }
                admin.observations.unregister(&removed_id);
                admin.observations.management_changed(Some(removed_id));
                Ok(())
            })
            .await
            .map_err(|error| ApiError::Internal(format!("remove Lustre registration: {error}")))??;
        }
        Change::Remove if resource.owner == Owner::Daemon => return Err(bad("应用管理挂载请使用卸载操作")),
        _ => return Err(bad("当前挂载由部署或外部运维管理")),
    }
    tracing::info!(%actor, filesystem = %id, "console updated Lustre filesystem");
    Ok(StatusCode::NO_CONTENT)
}

fn valid_name(name: &str) -> Result<String, ApiError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 255 || name.chars().any(char::is_control) {
        return Err(bad("请输入有效的集群名称"));
    }
    Ok(name.to_owned())
}

fn validate_mount_path(root: &Path, id: &FileSystemId, path: &Path) -> Result<(), ApiError> {
    if matches!(id.as_str(), "." | "..") {
        return Err(bad("无效的文件系统 ID"));
    }
    if path.parent() != Some(root)
        || !path.file_name().is_some_and(|name| {
            name.to_str().is_some_and(|name| {
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            })
        })
    {
        return Err(bad("挂载路径须为部署挂载目录的直接子目录"));
    }
    Ok(())
}

async fn registration(
    admin: &ResourceAdministration, request: RegisterRequest,
) -> Result<(Registration, bool), ApiError> {
    let name = valid_name(&request.name)?;
    let (path, source, owner, options, auto_mount, desired_mounted, mount_now) = match request.mount {
        NewMount::Existing { path } => {
            let source = read_mounts()?
                .remove(&path)
                .flatten()
                .ok_or_else(|| bad("该路径没有已挂载的 Lustre 文件系统"))?;
            (path, source, Owner::External, Vec::new(), false, false, false)
        }
        NewMount::Managed {
            filesystem,
            mgs_nids,
            path,
            options,
            auto_mount,
            mount_now,
        } => {
            if filesystem.is_empty()
                || filesystem.len() > 8
                || !filesystem
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                || mgs_nids.len() > 1024
                || !mgs_nids.contains('@')
                || mgs_nids.starts_with('-')
                || !mgs_nids
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._:@,[]-".contains(&byte))
            {
                return Err(bad("无效的 Lustre 文件系统名称或 MGS NID"));
            }
            if options.len() > 8
                || options.iter().any(|option| {
                    !matches!(
                        option.as_str(),
                        "flock" | "localflock" | "noflock" | "noatime" | "relatime" | "lazystatfs" | "verbose"
                    )
                })
            {
                return Err(bad(
                    "挂载参数仅支持 flock、localflock、noflock、noatime、relatime、lazystatfs、verbose",
                ));
            }
            let source = format!("{mgs_nids}:/{filesystem}");
            let mut restore_legacy = false;
            let path = match path {
                Some(path) if admin.uses_legacy_mount_path(&path) => {
                    let retired = admin
                        .records()
                        .await
                        .map_err(database_error)?
                        .into_iter()
                        .find(|entry| {
                            entry.removed
                                && entry.owner == Owner::Daemon
                                && entry.id == request.id
                                && entry.source == source
                                && entry.path == path
                        })
                        .ok_or_else(|| bad("旧版挂载路径仅用于恢复原有集群"))?;
                    restore_legacy = true;
                    admin.registered_mount_path(&retired)?
                }
                path => admin.mount_path(&request.id, path)?,
            };
            let mounts = read_mounts()?;
            if let Some(existing) = mounts.get(&path)
                && !(restore_legacy && existing.as_deref() == Some(source.as_str()))
            {
                return Err(bad("挂载路径已占用，请从已有挂载接入"));
            }
            (path, source, Owner::Daemon, options, auto_mount, mount_now, mount_now)
        }
    };
    Ok((
        Registration {
            id: request.id,
            name,
            owner,
            path,
            source,
            options,
            auto_mount,
            desired_mounted,
            unmount_pending: false,
            removed: false,
            tiers: BTreeMap::new(),
            last_mount_error: None,
        },
        mount_now,
    ))
}

fn decode_mount_field(value: &str) -> Option<String> {
    let mut result = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            result.push(byte);
            continue;
        }
        let a = bytes.next()?.checked_sub(b'0').filter(|digit| *digit < 4)?;
        let b = bytes.next()?.checked_sub(b'0').filter(|digit| *digit < 8)?;
        let c = bytes.next()?.checked_sub(b'0').filter(|digit| *digit < 8)?;
        result.push(a * 64 + b * 8 + c);
    }
    String::from_utf8(result).ok()
}

fn read_mounts() -> Result<BTreeMap<PathBuf, Option<String>>, ApiError> {
    let input = std::fs::read_to_string("/proc/self/mountinfo").map_err(system_error)?;
    Ok(input
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let path = decode_mount_field(left.split_ascii_whitespace().nth(4)?)?;
            let mut parts = right.split_ascii_whitespace();
            let fs = parts.next()?;
            let source = if fs == "lustre" {
                // A subtree bind can share the same source; register full roots only.
                if left.split_ascii_whitespace().nth(3) == Some("/") {
                    parts.next().and_then(decode_mount_field)
                } else {
                    None
                }
            } else {
                None
            };
            Some((PathBuf::from(path), source))
        })
        .collect())
}

pub(crate) fn registered_mount_present(path: &Path, source: &str) -> Result<bool, ApiError> {
    Ok(read_mounts()?.get(path).and_then(Option::as_deref) == Some(source))
}

fn mounted(resource: &Registration, mounts: &BTreeMap<PathBuf, Option<String>>) -> bool {
    mounts.get(&resource.path).and_then(Option::as_deref) == Some(resource.source.as_str())
}

async fn execute(mut command: Command) -> Result<(), ApiError> {
    let output = tokio::time::timeout(Duration::from_secs(30), command.kill_on_drop(true).output())
        .await
        .map_err(|_| ApiError::ServiceUnavailable("挂载命令超时".into()))?
        .map_err(system_error)?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let last = message.lines().rev().find(|line| !line.trim().is_empty());
        return Err(bad(&format!(
            "Lustre 命令失败：{}",
            last.unwrap_or("命令返回非零状态")
                .trim()
                .chars()
                .take(300)
                .collect::<String>()
        )));
    }
    Ok(())
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest(message.into())
}

fn database_error(error: sqlx::Error) -> ApiError {
    ApiError::Internal(error.to_string())
}

fn system_error(error: std::io::Error) -> ApiError {
    ApiError::ServiceUnavailable(format!("本机挂载服务失败：{error}"))
}
