//! HTTP request handlers for the robinhood-rs REST API.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use rbh_entry_store::model::{EntryKind, EntryRow, FileSystemId, ObjectId, ScopedEntryRow};
use rbh_entry_store::store::{AggregateKey, AggregateSort, QueryParam};
use rbh_policy::{PolicyError, rev8, store::Rev8PolicyRow};
use rbh_predicate::{Predicate, SortKey, SqlParam, to_sql};

use crate::{
    AppState,
    state::{ScanRecord, ScanState},
};

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/classifiers",
            post(crate::classifier_handlers::create_classifier).get(crate::classifier_handlers::list_classifiers),
        )
        .route(
            "/classifiers/{id}",
            get(crate::classifier_handlers::get_classifier)
                .put(crate::classifier_handlers::update_classifier)
                .delete(crate::classifier_handlers::delete_classifier),
        )
        .route(
            "/compat/lustre/classifiers/{id}/run",
            post(crate::classifier_handlers::run_classifier),
        )
        .route("/policies", post(create_policy).get(list_policies))
        .route(
            "/policies/{id}",
            get(get_policy).put(update_policy).delete(delete_policy),
        )
        .route("/policies/{id}/run", post(run_policy_now))
        .route("/policies/{id}/runs", get(list_rev8_runs))
        .route("/policies/{id}/breaker/reset", post(reset_rev8_breaker))
        .route("/policies/{id}/runs/{run}/operations", get(list_rev8_operations))
        .route("/policies/{id}/preview", post(preview_policy))
        .route("/policies/{id}/revisions/{revision}", get(get_policy_revision))
        .route("/entries/count", get(entry_count))
        .route("/entries/query", post(query_entries))
        .route("/entries/{object_id}", get(get_entry))
        .route("/reports/aggregate", post(report_aggregate))
        .route("/reports/top-size", get(top_size))
        .route("/reports/oldest", get(oldest_entries))
        .route("/reports/size-profile", get(size_profile))
        .route("/reports/stripe-distribution", get(stripe_distribution_report))
        .route("/reports/du", get(du_report))
        .route("/metrics", get(metrics_endpoint))
        .route("/removed", get(list_removed))
        .route("/removed/{fid}", axum::routing::delete(forget_removed))
        .route("/scans", post(start_scan).get(list_scans))
        .route("/scans/{id}", get(get_scan))
        .route("/compat/lustre/admin/dump", get(admin_dump))
        .route("/compat/lustre/admin/restore", post(admin_restore))
        .route("/compat/lustre/admin/sweep-orphans", post(admin_sweep_orphans))
        .route("/health", get(health))
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

// ---- /api/metrics ----
// Prometheus text exposition. Refreshes the catalog gauge on every
// scrape — the entries count is the only point-in-time metric that's
// not updated on a hot path.
async fn metrics_endpoint(State(state): State<AppState>) -> axum::response::Response {
    if let Ok(filesystems) = state.entry_store.list_filesystems().await {
        for filesystem in filesystems {
            if let Ok(count) = state.entry_store.scoped_entry_count(&filesystem.id).await {
                rbh_observability::metrics::CATALOG_ENTRIES
                    .with_label_values(&[filesystem.id.as_str(), filesystem.backend.as_str()])
                    .set(count as i64);
            }
        }
    }
    match rbh_observability::metrics::render() {
        Ok(body) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            body,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("metrics render error: {e}")).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

#[tracing::instrument(skip_all)]
async fn create_policy(
    State(state): State<AppState>, Extension(writers): Extension<crate::Rev8Writers>, headers: HeaderMap,
    Json(document): Json<serde_json::Value>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    rev8::validate_policy_version(&document)?;
    let actor = writers.authorize(&headers)?;
    let id = state
        .policy_store
        .save_rev8(&state.entry_store, None, &document, actor)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": id, "revision": 1 })),
    ))
}

#[tracing::instrument(skip(state))]
async fn list_policies(State(state): State<AppState>) -> Result<Json<Vec<Rev8PolicyRow>>, ApiError> {
    Ok(Json(state.policy_store.list_rev8().await?))
}

#[tracing::instrument(skip(state))]
async fn get_policy(State(state): State<AppState>, Path(id): Path<u64>) -> Result<Json<Rev8PolicyRow>, ApiError> {
    Ok(Json(
        state
            .policy_store
            .get_rev8(id)
            .await?
            .ok_or(PolicyError::NotFound(id))?,
    ))
}

#[tracing::instrument(skip_all, fields(id))]
async fn update_policy(
    State(state): State<AppState>, Path(id): Path<u64>, Extension(writers): Extension<crate::Rev8Writers>,
    headers: HeaderMap, Json(document): Json<serde_json::Value>,
) -> Result<StatusCode, ApiError> {
    rev8::validate_policy_version(&document)?;
    let actor = writers.authorize(&headers)?;
    state
        .policy_store
        .save_rev8(&state.entry_store, Some(id), &document, actor)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewQuery {
    heat_fid: Option<lustre_api::LuFid>,
}

#[tracing::instrument(skip(state))]
async fn preview_policy(
    State(state): State<AppState>, Path(id): Path<u64>, axum::extract::Query(query): axum::extract::Query<PreviewQuery>,
) -> Result<Json<rev8::PreviewResult>, ApiError> {
    Ok(Json(
        state
            .policy_store
            .preview_rev8_with_heat(&state.entry_store, id, query.heat_fid)
            .await?,
    ))
}

#[tracing::instrument(skip(state))]
async fn get_policy_revision(
    State(state): State<AppState>, Path((id, revision)): Path<(u64, u64)>,
) -> Result<Json<Rev8PolicyRow>, ApiError> {
    Ok(Json(state.policy_store.get_rev8_revision(id, revision).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    #[serde(default)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunPage {
    #[serde(default)]
    after_id: u64,
}

async fn reset_rev8_breaker(
    State(state): State<AppState>, Path(id): Path<u64>, Extension(writers): Extension<crate::Rev8Writers>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let actor = writers.authorize(&headers)?;
    state.policy_store.reset_rev8_breaker(id, actor).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_rev8_runs(
    State(state): State<AppState>, Path(id): Path<u64>, axum::extract::Query(page): axum::extract::Query<RunPage>,
) -> Result<Json<Vec<rbh_policy::store::Rev8Run>>, ApiError> {
    Ok(Json(state.policy_store.list_rev8_runs(id, page.after_id).await?))
}

async fn list_rev8_operations(
    State(state): State<AppState>, Path((id, run)): Path<(u64, u64)>,
    axum::extract::Query(page): axum::extract::Query<RunPage>,
) -> Result<Json<Vec<rbh_policy::store::Rev8Operation>>, ApiError> {
    Ok(Json(
        state.policy_store.list_rev8_operations(id, run, page.after_id).await?,
    ))
}

/// Queue a run bound to the current persisted policy revision.
#[tracing::instrument(skip_all, fields(id))]
async fn run_policy_now(
    State(state): State<AppState>, Path(id): Path<u64>, Extension(writers): Extension<crate::Rev8Writers>,
    headers: HeaderMap, body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let actor = writers.authorize(&headers)?;
    if !body.is_empty() {
        let request: RunRequest = serde_json::from_slice(&body)
            .map_err(|error| ApiError::BadRequest(format!("invalid Rev.8 run request: {error}")))?;
        if request.dry_run {
            return Ok(Json(state.policy_store.preview_rev8(&state.entry_store, id).await?).into_response());
        }
    }
    let run = state.policy_store.request_rev8_run(id, actor).await?;
    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"runId": run}))).into_response())
}

#[tracing::instrument(skip_all, fields(id))]
async fn delete_policy(
    State(state): State<AppState>, Path(id): Path<u64>, Extension(writers): Extension<crate::Rev8Writers>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    writers.authorize(&headers)?;
    state.policy_store.delete_rev8(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FilesystemQuery {
    filesystem: FileSystemId,
}

#[derive(Debug, Serialize)]
struct ScopedRows<T> {
    filesystem: FileSystemId,
    rows: Vec<T>,
}

#[tracing::instrument(skip(state))]
async fn entry_count(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<FilesystemQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state
        .entry_store
        .scoped_entry_count(&q.filesystem)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "filesystem": q.filesystem, "count": count })))
}

#[tracing::instrument(skip(state))]
async fn get_entry(
    State(state): State<AppState>, Path(object_id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<FilesystemQuery>,
) -> Result<Json<EntryDto>, ApiError> {
    let key = scoped_key_from_text(&state, q.filesystem, &object_id).await?;
    let entry = state
        .entry_store
        .get_scoped_entry(&key)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?
        .ok_or_else(|| ApiError::NotFound(format!("entry not found: {object_id}")))?;
    Ok(Json(entry.into()))
}

// ---- /api/entries/query ----

/// POST body for `/api/entries/query`.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
    /// Stable filesystem identity. Queries never implicitly span filesystems.
    pub filesystem: FileSystemId,
    /// Filter predicate. Omit / null = match all (`1=1`).
    #[serde(default)]
    pub predicate: Option<Predicate>,
    /// Zero or more sort keys, applied in the given order.
    #[serde(default)]
    pub order_by: Vec<SortKey>,
    /// Page size. Server caps at 10_000.
    #[serde(default = "default_limit")]
    pub limit: u64,
    #[serde(default)]
    pub offset: u64,
    /// Whether to compute the full match count (extra SQL roundtrip).
    #[serde(default)]
    pub with_total: bool,
}

fn default_limit() -> u64 {
    1000
}

#[derive(Debug, Serialize)]
pub struct QueryResponse {
    pub filesystem: FileSystemId,
    pub entries: Vec<EntryDto>,
    pub limit: u64,
    pub offset: u64,
    /// Present only when the request set `with_total=true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}

/// JSON-friendly view of `EntryRow`. Byte-string fields are rendered as
/// UTF-8 with lossy replacement (Lustre filenames are usually UTF-8 but
/// the byte layer does not enforce this).
#[derive(Debug, Serialize)]
pub struct EntryDto {
    pub filesystem: FileSystemId,
    pub object_id: ObjectId,
    pub parent_object_id: Option<ObjectId>,
    pub name: String,
    pub kind: &'static str,
    pub size: u64,
    pub blocks: u64,
    pub uid: u32,
    pub gid: u32,
    pub projid: u32,
    pub mode: u32,
    pub nlink: u32,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub stripe_count: Option<u16>,
    pub stripe_size: Option<u32>,
    pub stripe_items: Vec<u32>,
    pub pool_name: Option<String>,
    pub sm_status: serde_json::Value,
    pub last_seen: i64,
}

impl From<ScopedEntryRow> for EntryDto {
    fn from(r: ScopedEntryRow) -> Self {
        Self {
            filesystem: r.key.filesystem().clone(),
            object_id: *r.key.object(),
            parent_object_id: r.parent.as_ref().map(|key| *key.object()),
            name: String::from_utf8_lossy(&r.name).into_owned(),
            kind: entry_kind_str(r.kind),
            size: r.size,
            blocks: r.blocks,
            uid: r.uid,
            gid: r.gid,
            projid: r.projid,
            mode: r.mode,
            nlink: r.nlink,
            atime: r.atime,
            mtime: r.mtime,
            ctime: r.ctime,
            stripe_count: r.stripe_count,
            stripe_size: r.stripe_size,
            stripe_items: r.stripe_items,
            pool_name: r.pool_name,
            sm_status: r.sm_status,
            last_seen: r.last_seen,
        }
    }
}

fn entry_kind_str(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Directory => "dir",
        EntryKind::Symlink => "symlink",
        EntryKind::CharDevice => "chardev",
        EntryKind::BlockDevice => "blockdev",
        EntryKind::Fifo => "fifo",
        EntryKind::Socket => "socket",
    }
}

const MAX_LIMIT: u64 = 10_000;

#[tracing::instrument(skip(state, req), fields(limit = req.limit, offset = req.offset))]
async fn query_entries(
    State(state): State<AppState>, Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    let limit = req.limit.clamp(1, MAX_LIMIT);

    let (where_clause, params) = match &req.predicate {
        Some(p) => to_sql(p),
        None => ("1=1".to_string(), Vec::new()),
    };
    let store_params: Vec<QueryParam> = params
        .iter()
        .map(|p| match p {
            SqlParam::Num(n) => QueryParam::Int(*n),
            SqlParam::Str(s) => QueryParam::Str(s.clone()),
        })
        .collect();

    let order_by = if req.order_by.is_empty() {
        None
    } else {
        Some(SortKey::list_to_sql(&req.order_by))
    };

    let rows = state
        .entry_store
        .query_scoped_page(
            &req.filesystem,
            &where_clause,
            &store_params,
            order_by.as_deref(),
            limit,
            req.offset,
        )
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let total = if req.with_total {
        Some(
            state
                .entry_store
                .count_scoped_where(&req.filesystem, &where_clause, &store_params)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?,
        )
    } else {
        None
    };

    Ok(Json(QueryResponse {
        filesystem: req.filesystem,
        entries: rows.into_iter().map(EntryDto::from).collect(),
        limit,
        offset: req.offset,
        total,
    }))
}

// ---- /api/removed ----

#[derive(Debug, Deserialize)]
pub struct RemovedQuery {
    pub filesystem: FileSystemId,
    #[serde(default = "default_removed_limit")]
    pub limit: u64,
    #[serde(default)]
    pub offset: u64,
    /// Optional unix-epoch lower bound on rm_time.
    #[serde(default)]
    pub since: Option<i64>,
}

fn default_removed_limit() -> u64 {
    500
}

#[derive(Debug, Serialize)]
pub struct RemovedDto {
    pub filesystem: FileSystemId,
    pub object_id: ObjectId,
    pub parent_object_id: Option<ObjectId>,
    pub name: String,
    pub kind: &'static str,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub sm_status: serde_json::Value,
    pub rm_time: i64,
}

impl From<rbh_entry_store::model::ScopedRemovedEntry> for RemovedDto {
    fn from(r: rbh_entry_store::model::ScopedRemovedEntry) -> Self {
        let entry = r.entry;
        Self {
            filesystem: entry.key.filesystem().clone(),
            object_id: *entry.key.object(),
            parent_object_id: entry.parent.as_ref().map(|key| *key.object()),
            name: String::from_utf8_lossy(&entry.name).into_owned(),
            kind: entry_kind_str(entry.kind),
            size: entry.size,
            uid: entry.uid,
            gid: entry.gid,
            sm_status: entry.sm_status,
            rm_time: r.rm_time,
        }
    }
}

#[tracing::instrument(skip(state))]
async fn list_removed(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<RemovedQuery>,
) -> Result<Json<ScopedRows<RemovedDto>>, ApiError> {
    let limit = q.limit.clamp(1, 10_000);
    let rows = state
        .entry_store
        .list_scoped_removed(&q.filesystem, q.since, limit, q.offset)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(ScopedRows {
        filesystem: q.filesystem,
        rows: rows.into_iter().map(RemovedDto::from).collect(),
    }))
}

#[tracing::instrument(skip(state))]
async fn forget_removed(
    State(state): State<AppState>, Path(object_id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<FilesystemQuery>,
) -> Result<StatusCode, ApiError> {
    let key = scoped_key_from_text(&state, q.filesystem, &object_id).await?;
    let removed = state
        .entry_store
        .forget_scoped_removed(&key)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(if removed {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    })
}

async fn scoped_key_from_text(
    state: &AppState, filesystem: FileSystemId, value: &str,
) -> Result<rbh_entry_store::EntryKey, ApiError> {
    use std::str::FromStr;
    let config = state
        .entry_store
        .get_filesystem(&filesystem)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest(format!("unknown filesystem: {filesystem}")))?;
    let object = match config.backend {
        rbh_entry_store::BackendKind::Lustre => ObjectId::Lustre(
            lustre_api::LuFid::from_str(value.trim_matches(|c| c == '[' || c == ']'))
                .map_err(|e| ApiError::BadRequest(format!("invalid Lustre FID: {e}")))?,
        ),
        rbh_entry_store::BackendKind::JuiceFs => ObjectId::JuiceFs(
            value
                .parse()
                .map_err(|e| ApiError::BadRequest(format!("invalid JuiceFS inode: {e}")))?,
        ),
    };
    Ok(rbh_entry_store::EntryKey::new(filesystem, object))
}

// ---- /api/reports/* ----

#[derive(Debug, Deserialize)]
pub struct AggregateRequest {
    pub filesystem: FileSystemId,
    pub key: AggregateKey,
    #[serde(default = "default_agg_sort")]
    pub sort: AggSort,
    #[serde(default = "default_agg_limit")]
    pub limit: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggSort {
    Count,
    Size,
}

fn default_agg_sort() -> AggSort {
    AggSort::Size
}

fn default_agg_limit() -> u64 {
    50
}

#[derive(Debug, Serialize)]
pub struct AggregateRow {
    pub key: String,
    pub count: u64,
    pub total_size: u64,
    /// Friendly label derived from `key` when it's numeric:
    /// `uid` → passwd `pw_name`, `gid` → group `gr_name`. Missing or
    /// unresolved entries surface as `null` — callers should fall back
    /// to `key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Resolve a uid / gid to its name via NSS. Returns `None` when the id
/// isn't in passwd/group (or the lookup fails for any reason — we never
/// want a bad name lookup to break a report).
fn resolve_label(key: AggregateKey, numeric: &str) -> Option<String> {
    let n: u32 = numeric.parse().ok()?;
    match key {
        AggregateKey::Uid => nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(n))
            .ok()
            .flatten()
            .map(|u| u.name),
        AggregateKey::Gid => nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(n))
            .ok()
            .flatten()
            .map(|g| g.name),
        _ => None,
    }
}

#[tracing::instrument(skip(state, req))]
async fn report_aggregate(
    State(state): State<AppState>, Json(req): Json<AggregateRequest>,
) -> Result<Json<ScopedRows<AggregateRow>>, ApiError> {
    let sort = match req.sort {
        AggSort::Count => AggregateSort::Count,
        AggSort::Size => AggregateSort::Size,
    };
    let rows = state
        .entry_store
        .aggregate_scoped_by(&req.filesystem, req.key, sort, req.limit.clamp(1, 1_000))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let key_kind = req.key;
    Ok(Json(ScopedRows {
        filesystem: req.filesystem,
        rows: rows
            .into_iter()
            .map(|(key, count, total_size)| {
                let label = resolve_label(key_kind, &key);
                AggregateRow {
                    key,
                    count,
                    total_size,
                    label,
                }
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub filesystem: FileSystemId,
    #[serde(default = "default_top_n")]
    pub n: u64,
}

fn default_top_n() -> u64 {
    20
}

#[tracing::instrument(skip(state))]
async fn top_size(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<ScopedRows<EntryDto>>, ApiError> {
    let rows = state
        .entry_store
        .query_scoped_page(&q.filesystem, "1=1", &[], Some("size DESC"), q.n.clamp(1, 1_000), 0)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(ScopedRows {
        filesystem: q.filesystem,
        rows: rows.into_iter().map(EntryDto::from).collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct SizeBucket {
    pub bucket: String,
    pub count: u64,
    pub total_size: u64,
}

#[tracing::instrument(skip(state))]
async fn size_profile(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<FilesystemQuery>,
) -> Result<Json<ScopedRows<SizeBucket>>, ApiError> {
    let rows = state
        .entry_store
        .scoped_size_profile(&q.filesystem)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(ScopedRows {
        filesystem: q.filesystem,
        rows: rows
            .into_iter()
            .map(|(bucket, count, total_size)| SizeBucket {
                bucket,
                count,
                total_size,
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize)]
struct DuQuery {
    filesystem: FileSystemId,
    /// FID (literal, with or without surrounding brackets) to aggregate
    /// under. Mutually exclusive with `path`.
    #[serde(default)]
    fid: Option<String>,
    /// Absolute path (under the Lustre mount) resolved via
    /// `llapi_path2fid` on the daemon. Mutually exclusive with `fid`.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Serialize)]
struct DuResponse {
    filesystem: FileSystemId,
    object_id: ObjectId,
    file_count: u64,
    total_bytes: u64,
}

/// Recursive size aggregation under a FID subtree. Fast path: relies on
/// the catalog's `parent_fid` edge and a MariaDB recursive CTE — no
/// filesystem walk.
#[tracing::instrument(skip(state))]
async fn du_report(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<DuQuery>,
) -> Result<Json<DuResponse>, ApiError> {
    let root = match (q.fid.as_deref(), q.path.as_deref()) {
        (Some(value), None) => scoped_key_from_text(&state, q.filesystem.clone(), value).await?,
        (None, Some(p)) => {
            let namespace = rbh_namespace::NamespaceAdapter::new(state.entry_store.clone(), q.filesystem.clone())
                .await
                .map_err(|error| ApiError::BadRequest(error.to_string()))?;
            namespace
                .resolve(rbh_namespace::NamespaceTarget::Path(std::path::PathBuf::from(p)))
                .await
                .map_err(|error| ApiError::BadRequest(error.to_string()))?
                .key
        }
        _ => {
            return Err(ApiError::Internal("exactly one of fid / path must be provided".into()));
        }
    };
    let (file_count, total_bytes) = state
        .entry_store
        .scoped_subtree_totals(&root)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(DuResponse {
        filesystem: q.filesystem,
        object_id: *root.object(),
        file_count,
        total_bytes,
    }))
}

#[tracing::instrument(skip(state))]
async fn stripe_distribution_report(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<FilesystemQuery>,
) -> Result<Json<ScopedRows<rbh_entry_store::store::StripeDistRow>>, ApiError> {
    let rows = state
        .entry_store
        .scoped_stripe_distribution(&q.filesystem)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(ScopedRows {
        filesystem: q.filesystem,
        rows,
    }))
}

#[tracing::instrument(skip(state))]
async fn oldest_entries(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<ScopedRows<EntryDto>>, ApiError> {
    let rows = state
        .entry_store
        .query_scoped_page(&q.filesystem, "1=1", &[], Some("atime ASC"), q.n.clamp(1, 1_000), 0)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(ScopedRows {
        filesystem: q.filesystem,
        rows: rows.into_iter().map(EntryDto::from).collect(),
    }))
}

// ---- /api/scans ----

#[derive(Debug, Deserialize)]
pub struct StartScanRequest {
    pub filesystem: FileSystemId,
    /// Root directory to scan. Defaults to the daemon's configured mount.
    pub root: Option<String>,
    /// Incremental scan: only emit entries with mtime >= this unix time.
    #[serde(default)]
    pub since_mtime: Option<i64>,
    /// Globs to skip (merged with .rbh_ignore at the root).
    #[serde(default)]
    pub ignore_globs: Vec<String>,
    /// Worker count. Defaults to the crate default.
    #[serde(default)]
    pub concurrency: Option<usize>,
    /// Max directory depth; None = unlimited.
    #[serde(default)]
    pub max_depth: Option<usize>,
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[tracing::instrument(skip(state, req))]
async fn start_scan(
    State(state): State<AppState>, Json(req): Json<StartScanRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let config = state
        .entry_store
        .get_filesystem(&req.filesystem)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest(format!("unknown filesystem: {}", req.filesystem)))?;
    if config.backend != rbh_entry_store::BackendKind::Lustre {
        return Err(ApiError::BadRequest(
            "JuiceFS scans are coordinated by its baseline runtime".into(),
        ));
    }
    let root = req
        .root
        .clone()
        .unwrap_or_else(|| config.mount_path.to_string_lossy().into_owned());
    let id = uuid::Uuid::new_v4().to_string();

    // Record the full glob list visible to the walker so operators can
    // see what `.rbh_ignore` actually contributed.
    let mut merged_globs = req.ignore_globs.clone();
    merged_globs.extend(rbh_fs_scan::load_rbh_ignore_file(std::path::Path::new(&root)));

    let rec = ScanRecord {
        id: id.clone(),
        filesystem: req.filesystem.clone(),
        root: root.clone(),
        since_mtime: req.since_mtime,
        ignore_globs: merged_globs,
        state: ScanState::Running,
        scanned: 0,
        errors: 0,
        dirs: 0,
        started_at: now_epoch(),
        finished_at: None,
        error_message: None,
    };
    state.scans.lock().await.insert(id.clone(), rec);

    // Detach a worker that drains the scan into the entry store.
    let scans = state.scans.clone();
    let entry_store = state.entry_store.clone();
    let scan_id = id.clone();
    let filesystem = req.filesystem.clone();
    tokio::spawn(async move {
        let cfg = rbh_fs_scan::ScanConfig {
            root: std::path::PathBuf::from(&root),
            concurrency: req.concurrency.unwrap_or(4),
            max_depth: req.max_depth,
            channel_size: 4096,
            since_mtime: req.since_mtime,
            ignore_globs: req.ignore_globs,
        };
        let (mut rx, progress) = rbh_fs_scan::FsScanner::run(cfg);

        let mut batch: Vec<rbh_entry_store::model::EntryRow> = Vec::with_capacity(100);
        let mut errors = 0u64;
        while let Some(event) = rx.recv().await {
            match event {
                rbh_fs_scan::ScanEvent::Entry(entry) => {
                    batch.push(*entry);
                    if batch.len() >= 100 {
                        if let Err(e) = entry_store.upsert_lustre_scan_batch(&filesystem, &batch).await {
                            tracing::warn!(scan_id, error = %e, "batch upsert failed");
                            errors += 1;
                        }
                        batch.clear();
                    }
                }
                rbh_fs_scan::ScanEvent::Error { .. } => {
                    errors += 1;
                }
            }
        }
        if !batch.is_empty()
            && let Err(e) = entry_store.upsert_lustre_scan_batch(&filesystem, &batch).await
        {
            tracing::warn!(scan_id, error = %e, "final batch upsert failed");
            errors += 1;
        }

        let (scanned, scan_errors, dirs) = progress.snapshot();
        let total_errors = errors + scan_errors;
        let mut map = scans.lock().await;
        if let Some(rec) = map.get_mut(&scan_id) {
            rec.state = if total_errors > 0 && scanned == 0 {
                ScanState::Failed
            } else {
                ScanState::Completed
            };
            rec.scanned = scanned;
            rec.errors = total_errors;
            rec.dirs = dirs;
            rec.finished_at = Some(now_epoch());
        }
    });

    Ok((StatusCode::ACCEPTED, Json(serde_json::json!({"id": id}))))
}

#[tracing::instrument(skip(state))]
async fn list_scans(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<FilesystemQuery>,
) -> Json<Vec<ScanRecord>> {
    let map = state.scans.lock().await;
    let mut v: Vec<ScanRecord> = map
        .values()
        .filter(|record| record.filesystem == q.filesystem)
        .cloned()
        .collect();
    v.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    Json(v)
}

#[tracing::instrument(skip(state))]
async fn get_scan(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<ScanRecord>, ApiError> {
    let map = state.scans.lock().await;
    map.get(&id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::Internal(format!("scan not found: {id}")))
}

// ---------------------------------------------------------------------------
// Admin: catalog dump / restore
// ---------------------------------------------------------------------------

/// Stream the entire `entries` table as newline-delimited JSON
/// (application/x-ndjson). Rows are emitted in FID order so a dump
/// truncated mid-stream can be resumed client-side by filtering on the
/// last seen FID. Chunks of 1000 rows are fetched per DB round-trip.
///
/// Disaster-recovery use case: `rbh admin dump > catalog.ndjson`, then
/// after a rebuild `rbh admin restore < catalog.ndjson`.
async fn admin_dump(State(state): State<AppState>) -> axum::response::Response {
    use axum::body::Body;
    use futures_util::stream;
    use lustre_api::LuFid;

    const PAGE: u64 = 1000;
    let store = state.entry_store.clone();

    // Cursor state: `Some(after)` = continue scanning strictly after `after`,
    // `None` = finished, end of stream.
    let stream = stream::try_unfold(Some(None::<LuFid>), move |cursor| {
        let store = store.clone();
        async move {
            let Some(after) = cursor else {
                return Ok::<_, std::io::Error>(None);
            };
            let rows = store
                .legacy_lustre_dump_page(after, PAGE)
                .await
                .map_err(|e| std::io::Error::other(format!("legacy_lustre_dump_page: {e}")))?;
            if rows.is_empty() {
                return Ok(None);
            }
            let last = rows.last().map(|r| r.fid);
            let mut out = Vec::with_capacity(rows.len() * 256);
            for row in &rows {
                match serde_json::to_vec(row) {
                    Ok(mut b) => {
                        out.append(&mut b);
                        out.push(b'\n');
                    }
                    Err(e) => return Err(std::io::Error::other(format!("serialize: {e}"))),
                }
            }
            let next_cursor = if rows.len() < PAGE as usize { None } else { Some(last) };
            Ok(Some((axum::body::Bytes::from(out), next_cursor)))
        }
    });

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        Body::from_stream(stream),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct RestoreQuery {
    /// Process at most N lines per batch. Defaults to 500.
    #[serde(default = "default_restore_batch")]
    batch: usize,
}

fn default_restore_batch() -> usize {
    500
}

#[derive(Debug, Serialize)]
struct RestoreSummary {
    restored: u64,
    failed: u64,
}

/// Consume a newline-delimited JSON stream of `EntryRow`s and upsert
/// them back into the catalog. Silently skips blank lines; a batch that
/// fails to parse or upsert is logged and counted under `failed` but
/// does not abort the import.
async fn admin_restore(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<RestoreQuery>, body: axum::body::Bytes,
) -> Result<Json<RestoreSummary>, ApiError> {
    let batch_size = q.batch.clamp(1, 10_000);
    let mut restored = 0u64;
    let mut failed = 0u64;
    let mut batch: Vec<EntryRow> = Vec::with_capacity(batch_size);

    for (lineno, line) in body.split(|&b| b == b'\n').enumerate() {
        let has_content = line.iter().any(|b| !b.is_ascii_whitespace());
        if !has_content {
            continue;
        }
        match serde_json::from_slice::<EntryRow>(line) {
            Ok(row) => batch.push(row),
            Err(e) => {
                failed += 1;
                tracing::warn!(lineno = lineno + 1, error = %e, "restore: bad JSON line");
                continue;
            }
        }
        if batch.len() >= batch_size {
            match state.entry_store.legacy_lustre_upsert_batch(&batch).await {
                Ok(()) => restored += batch.len() as u64,
                Err(e) => {
                    failed += batch.len() as u64;
                    tracing::warn!(error = %e, "restore: batch upsert failed");
                }
            }
            batch.clear();
        }
    }
    if !batch.is_empty() {
        match state.entry_store.legacy_lustre_upsert_batch(&batch).await {
            Ok(()) => restored += batch.len() as u64,
            Err(e) => {
                failed += batch.len() as u64;
                tracing::warn!(error = %e, "restore: final batch upsert failed");
            }
        }
    }
    Ok(Json(RestoreSummary { restored, failed }))
}

#[derive(Debug, Deserialize)]
struct SweepOrphansQuery {
    /// Entries whose `last_seen < before` are candidates. Caller usually
    /// passes the Unix timestamp at which the most recent full scan
    /// started.
    before: i64,
    /// Cap per call. Default 5_000 so a single request doesn't lock the
    /// table for long.
    #[serde(default = "default_sweep_limit")]
    limit: u64,
    /// When true, counts candidates without moving them.
    #[serde(default)]
    dry_run: bool,
}

fn default_sweep_limit() -> u64 {
    5_000
}

#[derive(Debug, Serialize)]
struct SweepOrphansResponse {
    swept: u64,
    dry_run: bool,
}

/// Sweep stale entries into `removed_entries`. The caller typically runs
/// a full `rbh scan` first, then calls this with `before = scan_started_at`.
async fn admin_sweep_orphans(
    State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<SweepOrphansQuery>,
) -> Result<Json<SweepOrphansResponse>, ApiError> {
    let swept = state
        .entry_store
        .legacy_lustre_sweep_orphans(q.before, q.limit.clamp(1, 100_000), q.dry_run)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(SweepOrphansResponse {
        swept,
        dry_run: q.dry_run,
    }))
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum ApiError {
    Policy(PolicyError),
    Unauthorized,
    BadRequest(String),
    NotFound(String),
    Internal(String),
}

impl From<PolicyError> for ApiError {
    fn from(e: PolicyError) -> Self {
        Self::Policy(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            ApiError::Policy(PolicyError::NotFound(id)) => {
                (StatusCode::NOT_FOUND, format!("policy not found: id={id}"))
            }
            ApiError::Policy(PolicyError::InvalidTrigger(msg)) => (StatusCode::BAD_REQUEST, msg.clone()),
            ApiError::Policy(PolicyError::InvalidPolicy(msg)) => (StatusCode::BAD_REQUEST, msg.clone()),
            ApiError::Policy(error @ PolicyError::NameConflict(_)) => (StatusCode::CONFLICT, error.to_string()),
            ApiError::Policy(error @ PolicyError::ResourceInUse(_)) => (StatusCode::CONFLICT, error.to_string()),
            ApiError::Policy(error @ PolicyError::ResourceNotFound(_)) => (StatusCode::NOT_FOUND, error.to_string()),
            ApiError::Unauthorized => {
                return (
                    StatusCode::UNAUTHORIZED,
                    [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
                    Json(serde_json::json!({"error": "Rev.8 configuration requires an authenticated writer"})),
                )
                    .into_response();
            }
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            ApiError::Policy(e) => {
                tracing::error!(error = %e, "policy error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
            ApiError::Internal(msg) => {
                tracing::error!(error = %msg, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

#[cfg(test)]
pub(crate) mod scoped_api_tests {
    use super::*;
    use axum_test::TestServer;
    use bytes::Bytes;
    use rbh_entry_store::{EntryKey, ScopedEntryRow};
    use sqlx::mysql::MySqlPoolOptions;
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};

    pub(crate) fn state_without_database_connection() -> AppState {
        let pool = MySqlPoolOptions::new()
            .connect_lazy("mysql://root@127.0.0.1/rbh_entries_test")
            .unwrap();
        AppState {
            policy_store: rbh_policy::PolicyStore::new(pool.clone()),
            classifier_store: rbh_policy::ClassifierStore::new(pool.clone()),
            classifier_cache: Arc::new(RwLock::new(Vec::new())),
            entry_store: rbh_entry_store::EntryStore::with_pool(pool),
            scans: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn integration_enabled() -> bool {
        matches!(std::env::var("RBH_INTEGRATION"), Ok(value) if !value.is_empty() && value != "0")
    }

    pub(crate) fn rev8_test_server(state: AppState) -> TestServer {
        use sha2::{Digest, Sha256};

        const TOKEN: &str = "rev8-rest-test-writer";
        let digest = hex::encode(Sha256::digest(TOKEN));
        let writers =
            crate::Rev8Writers::from_digest_json(&serde_json::json!({"config-operator": digest}).to_string()).unwrap();
        let mut server = TestServer::new(crate::router_with_writers(state, writers)).unwrap();
        server.add_header(axum::http::header::AUTHORIZATION, format!("Bearer {TOKEN}"));
        server
    }

    pub(crate) fn resource_document(kind: &str, name: &str, spec: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "hsm.storage.io/v1alpha3", "kind": kind, "metadata": {"name": name}, "spec": spec
        })
    }

    pub(crate) fn rev8_archive_policy(filesystem: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "hsm.storage.io/v1alpha3",
            "kind": "Policy",
            "metadata": {"name": name},
            "spec": {
                "target": {"filesystem": filesystem, "objectType": "file"},
                "trigger": {"type": "periodic", "interval": "1h"},
                "objective": {"type": "exhaust"},
                "rules": {
                    "entries": [
                        {
                            "match": {"field": "object.size", "op": "gte", "value": 64},
                            "decision": "execute",
                            "action": {"type": "lustre.hsm.archive", "params": {"archiveId": 1}}
                        }
                    ]
                },
                "execution": {
                    "profileRef": "builtin-preview",
                    "activation": {"mode": "preview"},
                    "budget": {"run": {
                        "maxOperations": 10, "maxSubmitAttempts": 10, "maxObjectLogicalBytes": "1GiB"
                    }}
                },
                "governance": {"authorizationRef": "builtin-preview", "approvalPolicyRef": "builtin-preview"}
            }
        })
    }

    #[tokio::test]
    async fn rev8_policy_reports_unsupported_version_before_accessing_storage() {
        let server = rev8_test_server(state_without_database_connection());
        let mut document = rev8_archive_policy("lustre", "archive-preview");
        document["apiVersion"] = "hsm.storage.io/v1alpha2".into();
        let response = server.post("/api/policies").json(&document).await;
        response.assert_status_bad_request();
        let body: serde_json::Value = response.json();
        assert!(body["error"].as_str().unwrap().contains("apiVersion"));
    }

    #[tokio::test]
    async fn legacy_policy_submissions_require_a_rev8_document() {
        let server = rev8_test_server(state_without_database_connection());
        let legacy = serde_json::json!({
            "name": "old-archive", "filesystem": "lustre", "kind": "hsm_archive",
            "enabled": true, "trigger": "1h", "action": {"hsm": {"archive_id": 1}}
        });
        for request in [server.post("/api/policies"), server.put("/api/policies/123")] {
            let response = request.json(&legacy).await;
            response.assert_status_bad_request();
            let body = response.json::<serde_json::Value>();
            assert!(body["error"].as_str().unwrap().contains("hsm.storage.io/v1alpha3"));
        }
    }

    #[tokio::test]
    async fn rev8_manual_run_rejects_a_legacy_target_even_without_a_json_content_type() {
        let server = rev8_test_server(state_without_database_connection());
        let response = server
            .post("/api/policies/123/run")
            .text(r#"{"target":{"kind":"ost","osts":[7]}}"#)
            .await;
        response.assert_status_bad_request();
        assert!(
            response.json::<serde_json::Value>()["error"]
                .as_str()
                .unwrap()
                .contains("target")
        );
    }

    #[tokio::test]
    async fn rev8_resource_write_requires_an_authenticated_identity() {
        let server = TestServer::new(crate::router(state_without_database_connection())).unwrap();
        let document = serde_json::json!({
            "apiVersion": "hsm.storage.io/v1alpha3", "kind": "Selector",
            "metadata": {"name": "all-files"}, "spec": {"where": true}
        });
        server
            .post("/api/selectors")
            .add_header("x-rbh-principal", "administrator")
            .json(&document)
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rev8_action_contract_is_registered_and_read_only() {
        let server = rev8_test_server(state_without_database_connection());
        let response = server.get("/api/action-contracts").await;
        response.assert_status_ok();
        let contracts = response.json::<Vec<serde_json::Value>>();
        assert_eq!(contracts.len(), 4);
        let migration = server.get("/api/action-contracts/lustre.pool_migrate").await;
        migration.assert_status_ok();
        let migration = migration.json::<serde_json::Value>();
        assert_eq!(
            migration["parametersSchema"]["required"],
            serde_json::json!(["sourcePool", "targetPool", "maxFileBytes"])
        );
        assert_eq!(migration["revision"], 2);
        assert_eq!(
            migration["parametersSchema"]["properties"]["minResidence"]["type"],
            "string"
        );
        let previous_migration = server
            .get("/api/action-contracts/lustre.pool_migrate/revisions/1")
            .await;
        previous_migration.assert_status_ok();
        assert!(
            previous_migration.json::<serde_json::Value>()["parametersSchema"]["properties"]
                .get("minResidence")
                .is_none()
        );
        assert_eq!(
            migration["definition"]["spec"]["completionGuarantees"][0]["evidence"],
            "verified_lustre_layout"
        );
        let url = "/api/action-contracts/lustre.hsm.archive";
        let entry = server.get(url).await;
        entry.assert_status_ok();
        assert_eq!(entry.json::<serde_json::Value>(), contracts[0]);
        assert_eq!(contracts[0]["definition"]["spec"]["effect"]["category"], "data_copy");
        let historical = server.get(&format!("{url}/revisions/1")).await;
        historical.assert_status_ok();
        assert_eq!(historical.json::<serde_json::Value>()["revision"], 1);
        assert_eq!(contracts[0]["revision"], 2);
        server
            .put(url)
            .json(&contracts[0])
            .await
            .assert_status(StatusCode::METHOD_NOT_ALLOWED);
        server.delete(url).await.assert_status(StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn rev8_policy_reports_invalid_configuration_at_its_field() {
        let server = rev8_test_server(state_without_database_connection());
        for (field, value, error_field) in [
            ("/spec/execution/activation/mode", "unknown".into(), "activation.mode"),
            ("/spec/trigger/interval", "0h".into(), "trigger.interval"),
        ] {
            let mut document = rev8_archive_policy("lustre", "archive-preview");
            *document.pointer_mut(field).unwrap() = value;
            let response = server.post("/api/policies").json(&document).await;
            response.assert_status_bad_request();
            let body: serde_json::Value = response.json();
            let message = body["error"].as_str().unwrap();
            assert!(message.contains(error_field), "{field}: {message}");
        }

        let mut document = rev8_archive_policy("lustre", "archive-preview");
        document["spec"]["execution"]["concurrency"] = 1.into();
        let response = server.post("/api/policies").json(&document).await;
        response.assert_status_bad_request();
        let body: serde_json::Value = response.json();
        assert!(body["error"].as_str().unwrap().contains("unknown field `concurrency`"));
    }

    #[tokio::test]
    async fn rev8_policy_previews_scoped_candidates_and_preserves_the_latest_revision() {
        if !integration_enabled() {
            return;
        }
        let database_url =
            std::env::var("RBH_TEST_DATABASE_URL").unwrap_or_else(|_| "mysql://root@localhost/rbh_entries_test".into());
        let store = rbh_entry_store::EntryStore::connect(&database_url).await.unwrap();
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..12];
        let first = FileSystemId::new(format!("preview-a-{suffix}")).unwrap();
        let second = FileSystemId::new(format!("preview-b-{suffix}")).unwrap();
        for filesystem in [&first, &second] {
            store
                .register_filesystem(&rbh_entry_store::FileSystemConfig {
                    id: filesystem.clone(),
                    backend: rbh_entry_store::BackendKind::Lustre,
                    mount_path: format!("/mnt/{filesystem}").into(),
                    capabilities: rbh_entry_store::BackendCapabilities {
                        hsm: true,
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
        }
        for (fs, oid, kind, size) in [
            (&first, 1, EntryKind::File, 64),
            (&first, 2, EntryKind::File, 128),
            (&first, 3, EntryKind::File, 32),
            (&first, 4, EntryKind::Directory, 4096),
            (&second, 1, EntryKind::File, 512),
        ] {
            store
                .upsert_scoped_entry(&ScopedEntryRow {
                    key: EntryKey::new(
                        fs.clone(),
                        ObjectId::Lustre(lustre_api::LuFid::new(0x200000401, oid, 0)),
                    ),
                    parent: None,
                    name: Bytes::from(format!("entry-{oid}")),
                    kind,
                    size,
                    blocks: 1,
                    uid: 1,
                    gid: 1,
                    projid: 0,
                    mode: 0o644,
                    nlink: 1,
                    atime: 0,
                    mtime: 0,
                    ctime: 0,
                    stripe_count: None,
                    stripe_size: None,
                    stripe_items: Vec::new(),
                    pool_name: None,
                    sm_status: serde_json::json!({}),
                    last_seen: 0,
                    depth: 1,
                })
                .await
                .unwrap();
        }
        let build_server = |entry_store: rbh_entry_store::EntryStore| {
            let mut state = state_without_database_connection();
            state.policy_store = rbh_policy::PolicyStore::new(entry_store.pool().clone());
            state.classifier_store = rbh_policy::ClassifierStore::new(entry_store.pool().clone());
            state.entry_store = entry_store;
            rev8_test_server(state)
        };
        let server = build_server(store);
        let mut document = rev8_archive_policy(first.as_str(), &format!("archive-preview-{suffix}"));
        for (field, value, error_field) in [
            ("/spec/execution/profileRef", "unknown".into(), "profileRef"),
            ("/spec/rules/entries/0/match/field", "object.fizz".into(), "match.field"),
            ("/spec/rules/entries/0/match/op", "less".into(), "match.op"),
            (
                "/spec/rules/entries/0/match",
                serde_json::json!({"all": [true, {"field": "object.size", "op": "less", "value": 4}]}),
                "match.all[1].op",
            ),
        ] {
            let mut invalid = document.clone();
            *invalid.pointer_mut(field).unwrap() = value;
            let response = server.post("/api/policies").json(&invalid).await;
            response.assert_status_bad_request();
            let body = response.json::<serde_json::Value>();
            let message = body["error"].as_str().unwrap();
            assert!(message.contains(error_field), "{field}: {message}");
        }
        let created = server.post("/api/policies").json(&document).await;
        created.assert_status(StatusCode::CREATED);
        let result: serde_json::Value = created.json();
        let id = result["id"].as_u64().unwrap();
        let policy_url = format!("/api/policies/{id}");
        let preview_url = format!("{policy_url}/preview");
        let preview = server.post(&preview_url).await;
        preview.assert_status_ok();
        let preview = preview.json::<serde_json::Value>();
        assert_eq!(preview["revision"], 1);
        assert_eq!(preview["candidateCount"], 2);
        assert_eq!(preview["estimatedBytes"], 192);

        let rules = document["spec"]["rules"]["entries"].as_array_mut().unwrap();
        rules.push(serde_json::json!({
            "priority": 10,
            "match": {"field": "object.size", "op": "gt", "value": 64},
            "decision": "skip"
        }));
        server
            .put(&policy_url)
            .json(&document)
            .await
            .assert_status(StatusCode::NO_CONTENT);
        drop(server);
        let reopened = rbh_entry_store::EntryStore::connect(&database_url).await.unwrap();
        let server = build_server(reopened);
        let current = server.get(&policy_url).await;
        current.assert_status_ok();
        let current: serde_json::Value = current.json();
        assert_eq!(current["revision"], 2);
        assert_eq!(current["definition"], document);
        let list = server.get("/api/policies").await;
        list.assert_status_ok();
        assert!(list.json::<Vec<serde_json::Value>>().contains(&current));
        let preview = server.post(&preview_url).await;
        preview.assert_status_ok();
        let preview = preview.json::<serde_json::Value>();
        assert_eq!(preview["revision"], 2);
        assert_eq!(preview["candidateCount"], 1);
        assert_eq!(preview["estimatedBytes"], 64);

        check_resource_bindings(&server, &first, suffix, &policy_url, &preview_url, document).await;
        server.delete(&policy_url).await.assert_status(StatusCode::NO_CONTENT);
        server.get(&policy_url).await.assert_status_not_found();
        server
            .get(&format!("{policy_url}/revisions/1"))
            .await
            .assert_status_ok();
    }

    async fn check_resource_bindings(
        server: &TestServer, filesystem: &FileSystemId, suffix: &str, policy_url: &str, preview_url: &str,
        mut policy: serde_json::Value,
    ) {
        use serde_json::{Value, json};

        let name = format!("config-{suffix}");
        let alias = format!("alias-{suffix}");
        let resources = [
            (
                "selectors",
                "Selector",
                json!({"where": {"field": "object.size", "op": "gte", "value": 64}}),
            ),
            (
                "execution-profiles",
                "ExecutionProfile",
                json!({
                    "concurrency": {"global": 1},
                    "retry": {"maxAttempts": 1, "backoff": "fixed", "initialDelay": "1s", "maxDelay": "1s", "retryableErrors": ["transient_io"]},
                    "workerLease": {"ttl": "1m", "renewEvery": "10s", "onLeaseLost": "stop_and_reconcile"},
                    "circuitBreaker": {"scopes": [{"domain": {"key": "policy"}, "errorClasses": ["transient_io"], "evaluation": "any",
                        "conditions": [{"type": "consecutive_failures", "threshold": 1}], "onOpen": "suspend_submissions", "resetMode": "manual"}]}
                }),
            ),
            (
                "authorization-policies",
                "AuthorizationPolicy",
                json!({
                    "defaultDecision": "deny", "statements": [{"id": "archive", "decision": "allow",
                        "subjects": {"roles": ["archive-admin"]}, "actions": ["lustre.hsm.archive"], "scope": {"filesystems": [filesystem]}}]
                }),
            ),
            (
                "approval-policies",
                "ApprovalPolicy",
                json!({"defaultDecision": "deny", "requirements": [
                    {"id": "archive", "actions": ["lustre.hsm.archive"], "mode": "not_required"}
                ]}),
            ),
            (
                "system-safety-policies",
                "SystemSafetyPolicy",
                json!({
                    "scope": {"filesystems": [filesystem]}, "limits": {"maxOperations": 10}
                }),
            ),
        ];
        for (collection, kind, spec) in resources {
            let base = format!("/api/{collection}");
            let url = format!("{base}/{name}");
            let mut document = resource_document(kind, &name, spec);
            let response = server
                .post(&base)
                .add_header("x-rbh-principal", "spoofed")
                .json(&document)
                .await;
            response.assert_status(StatusCode::CREATED);
            let created = response.json::<Value>();
            assert_eq!(created["modifiedBy"], "config-operator");
            assert_eq!(created["revision"], 1);
            let list = server.get(&base).await;
            list.assert_status_ok();
            assert!(list.json::<Vec<Value>>().contains(&created));
            document["metadata"]["description"] = "revision two".into();
            let response = server.put(&url).json(&document).await;
            response.assert_status_ok();
            assert_eq!(response.json::<Value>()["revision"], 2);
            let history = server.get(&format!("{url}/revisions/1")).await;
            history.assert_status_ok();
            assert_eq!(history.json::<Value>(), created);
        }
        let auto_rebound = server.get(policy_url).await;
        auto_rebound.assert_status_ok();
        assert!(
            auto_rebound.json::<Value>()["resourceBindings"]
                .as_array()
                .unwrap()
                .contains(&json!({"kind": "SystemSafetyPolicy", "name": name, "revision": 2}))
        );
        let nested = resource_document("Selector", &alias, json!({"where": {"selectorRef": name}}));
        server
            .post("/api/selectors")
            .json(&nested)
            .await
            .assert_status(StatusCode::CREATED);
        let selector_url = format!("/api/selectors/{name}");
        let cycle = resource_document("Selector", &name, json!({"where": {"selectorRef": alias}}));
        let response = server.put(&selector_url).json(&cycle).await;
        response.assert_status_bad_request();
        assert!(response.json::<Value>()["error"].as_str().unwrap().contains("cycle"));
        let invalid = resource_document(
            "ApprovalPolicy",
            &name,
            json!({"defaultDecision": "deny", "requirements": [
                {"id": "archive", "actions": ["lustre.hsm.archive"], "mode": "not_required", "ttl": "1h"}
            ]}),
        );
        server
            .put(&format!("/api/approval-policies/{name}"))
            .json(&invalid)
            .await
            .assert_status_bad_request();

        policy["spec"]["target"]["where"] = json!({"selectorRef": alias});
        policy["spec"]["execution"]["profileRef"] = name.clone().into();
        policy["spec"]["governance"]["authorizationRef"] = name.clone().into();
        policy["spec"]["governance"]["approvalPolicyRef"] = name.clone().into();
        server
            .put(policy_url)
            .json(&policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let linked_preview = server.post(preview_url).await;
        linked_preview.assert_status_ok();
        let linked_preview = linked_preview.json::<Value>();
        assert_eq!(linked_preview["candidateCount"], 1);
        let bindings = linked_preview["resourceBindings"].as_array().unwrap();
        assert_eq!(bindings.len(), 7);
        assert!(bindings.contains(&json!({"kind": "Selector", "name": name, "revision": 2})));
        assert!(bindings.contains(&json!({"kind": "SystemSafetyPolicy", "name": name, "revision": 2})));
        let contract = bindings
            .iter()
            .find(|binding| binding["kind"] == "ActionContract")
            .unwrap();
        assert_eq!(contract["name"], "lustre.hsm.archive");
        assert_eq!(contract["revision"], 2);
        assert_eq!(contract["serverDefinition"]["metadata"]["name"], "lustre.hsm.archive");

        let human_floor = resource_document(
            "SystemSafetyPolicy",
            &name,
            json!({
                "scope": {"filesystems": [filesystem]}, "effectRules": [{
                    "effect": "data_copy", "minimumApproval": "human", "allowAutoBreakerReset": false
                }]
            }),
        );
        let safety_url = format!("/api/system-safety-policies/{name}");
        let incompatible = server.put(&safety_url).json(&human_floor).await;
        incompatible.assert_status_bad_request();
        assert!(
            incompatible.json::<Value>()["error"]
                .as_str()
                .unwrap()
                .contains("APPROVAL_FLOOR_UNSATISFIABLE")
        );
        assert_eq!(server.get(&safety_url).await.json::<Value>()["revision"], 2);

        let changed = resource_document(
            "Selector",
            &name,
            json!({"where": {"field": "object.size", "op": "gt", "value": 64}}),
        );
        server.put(&selector_url).json(&changed).await.assert_status_ok();
        let pinned = server.post(preview_url).await;
        pinned.assert_status_ok();
        assert_eq!(pinned.json::<Value>(), linked_preview);
        server
            .put(policy_url)
            .json(&policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let rebound = server.post(preview_url).await;
        rebound.assert_status_ok();
        assert_eq!(rebound.json::<Value>()["candidateCount"], 0);
        let linked_revision = linked_preview["revision"].as_u64().unwrap();
        let old = server.get(&format!("{policy_url}/revisions/{linked_revision}")).await;
        old.assert_status_ok();
        assert_eq!(
            old.json::<Value>()["resourceBindings"],
            linked_preview["resourceBindings"]
        );

        let profile_url = format!("/api/execution-profiles/{name}");
        server.delete(&profile_url).await.assert_status(StatusCode::CONFLICT);
        server
            .delete(&format!("{profile_url}%20"))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        server.delete(&selector_url).await.assert_status(StatusCode::CONFLICT);
        policy["spec"]["enabled"] = false.into();
        server
            .put(policy_url)
            .json(&policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);
        for collection in [
            "execution-profiles",
            "authorization-policies",
            "approval-policies",
            "system-safety-policies",
        ] {
            let url = format!("/api/{collection}/{name}");
            server.delete(&url).await.assert_status(StatusCode::NO_CONTENT);
            server.get(&url).await.assert_status(StatusCode::NOT_FOUND);
            let tombstone = server.get(&format!("{url}/revisions/3")).await;
            tombstone.assert_status_ok();
            let tombstone = tombstone.json::<Value>();
            assert_eq!(tombstone["modifiedBy"], "config-operator");
            assert!(tombstone["definition"].is_null());
        }
        server
            .delete(&format!("/api/selectors/{alias}"))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        server.delete(&selector_url).await.assert_status(StatusCode::NO_CONTENT);
        server
            .get(&format!("{selector_url}/revisions/1"))
            .await
            .assert_status_ok();
        let historical = server.post(preview_url).await;
        historical.assert_status_ok();
        assert_eq!(historical.json::<Value>()["candidateCount"], 0);
    }

    #[tokio::test]
    async fn catalog_endpoints_reject_an_implicit_global_scope() {
        let server = TestServer::new(crate::router(state_without_database_connection())).unwrap();
        server.get("/api/entries/count").await.assert_status_bad_request();
        server.get("/api/reports/top-size").await.assert_status_bad_request();
        server.get("/api/removed").await.assert_status_bad_request();
        server
            .post("/api/entries/query")
            .json(&serde_json::json!({"predicate": null}))
            .await
            .assert_status_unprocessable_entity();
    }

    #[test]
    fn public_entry_identity_contains_filesystem_and_backend_native_object() {
        let filesystem = FileSystemId::new("juice-a").unwrap();
        let row = ScopedEntryRow {
            key: EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(42)),
            parent: Some(EntryKey::new(filesystem, ObjectId::JuiceFs(1))),
            name: Bytes::from_static(b"report.txt"),
            kind: EntryKind::File,
            size: 7,
            blocks: 8,
            uid: 1,
            gid: 2,
            projid: 3,
            mode: 0o644,
            nlink: 1,
            atime: 4,
            mtime: 5,
            ctime: 6,
            stripe_count: None,
            stripe_size: None,
            stripe_items: Vec::new(),
            pool_name: None,
            sm_status: serde_json::json!({}),
            last_seen: 7,
            depth: 1,
        };
        let value = serde_json::to_value(EntryDto::from(row)).unwrap();
        assert_eq!(value["filesystem"], "juice-a");
        assert_eq!(value["object_id"], serde_json::json!({"juice_fs": 42}));
        assert_eq!(value["parent_object_id"], serde_json::json!({"juice_fs": 1}));
        assert!(value.get("fid").is_none());
    }

    #[tokio::test]
    async fn query_api_returns_same_inode_independently_per_filesystem() {
        if !integration_enabled() {
            return;
        }
        let store = rbh_entry_store::EntryStore::connect("mysql://root@localhost/rbh_entries_test")
            .await
            .unwrap();
        let first = FileSystemId::new("api-scope-a").unwrap();
        let second = FileSystemId::new("api-scope-b").unwrap();
        for filesystem in [&first, &second] {
            store
                .register_filesystem(&rbh_entry_store::FileSystemConfig {
                    id: filesystem.clone(),
                    backend: rbh_entry_store::BackendKind::JuiceFs,
                    mount_path: format!("/mnt/{filesystem}").into(),
                    capabilities: rbh_entry_store::BackendCapabilities {
                        namespace: true,
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
        }
        for (filesystem, name, size) in [(&first, b"api-a".as_slice(), 11), (&second, b"api-b".as_slice(), 22)] {
            store
                .upsert_scoped_entry(&ScopedEntryRow {
                    key: EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(42)),
                    parent: None,
                    name: Bytes::copy_from_slice(name),
                    kind: EntryKind::File,
                    size,
                    blocks: 1,
                    uid: 1,
                    gid: 2,
                    projid: 3,
                    mode: 0o644,
                    nlink: 1,
                    atime: 4,
                    mtime: 5,
                    ctime: 6,
                    stripe_count: None,
                    stripe_size: None,
                    stripe_items: Vec::new(),
                    pool_name: None,
                    sm_status: serde_json::json!({}),
                    last_seen: 7,
                    depth: 1,
                })
                .await
                .unwrap();
        }
        let pool = store.pool().clone();
        let state = AppState {
            policy_store: rbh_policy::PolicyStore::new(pool.clone()),
            classifier_store: rbh_policy::ClassifierStore::new(pool),
            classifier_cache: Arc::new(RwLock::new(Vec::new())),
            entry_store: store,
            scans: Arc::new(Mutex::new(HashMap::new())),
        };
        let server = TestServer::new(crate::router(state)).unwrap();
        for (filesystem, expected_name, expected_size) in [(&first, "api-a", 11_u64), (&second, "api-b", 22_u64)] {
            let response = server
                .post("/api/entries/query")
                .json(&serde_json::json!({"filesystem": filesystem, "limit": 10}))
                .await;
            response.assert_status_ok();
            let body: serde_json::Value = response.json();
            assert_eq!(body["filesystem"], filesystem.as_str());
            assert_eq!(body["entries"].as_array().unwrap().len(), 1);
            assert_eq!(body["entries"][0]["name"], expected_name);
            assert_eq!(body["entries"][0]["size"], expected_size);
            assert_eq!(body["entries"][0]["object_id"], serde_json::json!({"juice_fs": 42}));

            let report = server
                .get(&format!("/api/reports/top-size?filesystem={filesystem}&n=10"))
                .await;
            report.assert_status_ok();
            let report_body: serde_json::Value = report.json();
            assert_eq!(report_body["filesystem"], filesystem.as_str());
            assert_eq!(report_body["rows"].as_array().unwrap().len(), 1);
            assert_eq!(report_body["rows"][0]["name"], expected_name);
        }
    }

    #[tokio::test]
    async fn du_path_uses_juicefs_namespace_adapter() {
        if !integration_enabled() {
            return;
        }
        let store = rbh_entry_store::EntryStore::connect("mysql://root@localhost/rbh_entries_test")
            .await
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mount = temp.path().join("juice");
        std::fs::create_dir(&mount).unwrap();
        let path = mount.join("du-file");
        std::fs::write(&path, b"1234567").unwrap();
        let filesystem = FileSystemId::new(format!("api-du-{}", std::process::id())).unwrap();
        store
            .register_filesystem(&rbh_entry_store::FileSystemConfig {
                id: filesystem.clone(),
                backend: rbh_entry_store::BackendKind::JuiceFs,
                mount_path: mount.clone(),
                capabilities: rbh_entry_store::BackendCapabilities {
                    namespace: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let root = EntryKey::new(
            filesystem.clone(),
            ObjectId::JuiceFs(std::fs::metadata(&mount).unwrap().ino()),
        );
        let object_inode = std::fs::metadata(&path).unwrap().ino();
        let object = EntryKey::new(filesystem.clone(), ObjectId::JuiceFs(object_inode));
        for (key, parent, name, kind, size) in [
            (root.clone(), None, b"".as_slice(), EntryKind::Directory, 0),
            (
                object.clone(),
                Some(root.clone()),
                b"du-file".as_slice(),
                EntryKind::File,
                7,
            ),
        ] {
            store
                .upsert_scoped_entry(&ScopedEntryRow {
                    key,
                    parent,
                    name: Bytes::copy_from_slice(name),
                    kind,
                    size,
                    blocks: 1,
                    uid: 0,
                    gid: 0,
                    projid: 0,
                    mode: 0o644,
                    nlink: 1,
                    atime: 0,
                    mtime: 0,
                    ctime: 0,
                    stripe_count: None,
                    stripe_size: None,
                    stripe_items: Vec::new(),
                    pool_name: None,
                    sm_status: serde_json::json!({}),
                    last_seen: 0,
                    depth: if kind == EntryKind::File { 1 } else { 0 },
                })
                .await
                .unwrap();
        }
        store
            .upsert_scoped_namespace_edge(&rbh_entry_store::model::ScopedNamespaceEdge {
                filesystem: filesystem.clone(),
                parent: *root.object(),
                name: Bytes::from_static(b"du-file"),
                object: *object.object(),
            })
            .await
            .unwrap();
        let pool = store.pool().clone();
        let server = TestServer::new(crate::router(AppState {
            policy_store: rbh_policy::PolicyStore::new(pool.clone()),
            classifier_store: rbh_policy::ClassifierStore::new(pool),
            classifier_cache: Arc::new(RwLock::new(Vec::new())),
            entry_store: store,
            scans: Arc::new(Mutex::new(HashMap::new())),
        }))
        .unwrap();
        let response = server
            .get(&format!(
                "/api/reports/du?filesystem={filesystem}&path={}",
                path.display()
            ))
            .await;
        response.assert_status_ok();
        let body: serde_json::Value = response.json();
        assert_eq!(body["filesystem"], filesystem.as_str());
        assert_eq!(body["object_id"], serde_json::json!({"juice_fs": object_inode}));
        assert_eq!(body["file_count"], 1);
        assert_eq!(body["total_bytes"], 7);
    }

    #[tokio::test]
    async fn rev8_rejects_hsm_on_juicefs() {
        if !integration_enabled() {
            return;
        }
        let store = rbh_entry_store::EntryStore::connect("mysql://root@localhost/rbh_entries_test")
            .await
            .unwrap();
        let filesystem = FileSystemId::new(format!("policy-jfs-{}", std::process::id())).unwrap();
        store
            .register_filesystem(&rbh_entry_store::FileSystemConfig {
                id: filesystem.clone(),
                backend: rbh_entry_store::BackendKind::JuiceFs,
                mount_path: "/jfs".into(),
                capabilities: rbh_entry_store::BackendCapabilities {
                    namespace: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let pool = store.pool().clone();
        let server = rev8_test_server(AppState {
            policy_store: rbh_policy::PolicyStore::new(pool.clone()),
            classifier_store: rbh_policy::ClassifierStore::new(pool),
            classifier_cache: Arc::new(RwLock::new(Vec::new())),
            entry_store: store,
            scans: Arc::new(Mutex::new(HashMap::new())),
        });
        let document = rev8_archive_policy(filesystem.as_str(), "invalid-hsm");
        let response = server.post("/api/policies").json(&document).await;
        response.assert_status_bad_request();
        let body: serde_json::Value = response.json();
        assert!(body["error"].as_str().unwrap().contains("Lustre HSM"));
    }
}
