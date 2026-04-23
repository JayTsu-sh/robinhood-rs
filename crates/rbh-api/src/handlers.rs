//! HTTP request handlers for the robinhood-rs REST API.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use rbh_entry_store::model::{EntryKind, EntryRow};
use rbh_entry_store::store::{AggregateKey, AggregateSort, QueryParam};
use rbh_policy::{PolicyDef, PolicyError};
use rbh_predicate::{Predicate, SortKey, SqlParam, to_sql};

use crate::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/policies", post(create_policy).get(list_policies))
        .route(
            "/policies/{id}",
            get(get_policy).put(update_policy).delete(delete_policy),
        )
        .route("/entries/count", get(entry_count))
        .route("/entries/query", post(query_entries))
        .route("/reports/aggregate", post(report_aggregate))
        .route("/reports/top-size", get(top_size))
        .route("/reports/oldest", get(oldest_entries))
        .route("/metrics", get(metrics_endpoint))
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
    if let Ok(count) = state.entry_store.entry_count().await {
        rbh_observability::metrics::CATALOG_ENTRIES.set(count as i64);
    }
    match rbh_observability::metrics::render() {
        Ok(body) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            body,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("metrics render error: {e}"),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Policies
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct PolicyResponse {
    id: u64,
    name: String,
    kind: String,
    enabled: bool,
    definition: PolicyDef,
}

impl From<rbh_policy::PolicyRow> for PolicyResponse {
    fn from(row: rbh_policy::PolicyRow) -> Self {
        let kind = row.kind().as_str().to_string();
        Self {
            id: row.id,
            name: row.name,
            kind,
            enabled: row.enabled,
            definition: row.definition,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CreatePolicyRequest {
    #[serde(flatten)]
    definition: PolicyDef,
}

#[tracing::instrument(skip(state, body))]
async fn create_policy(
    State(state): State<AppState>, Json(body): Json<CreatePolicyRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id = state.policy_store.create(&body.definition).await?;
    // Reconcile triggers → scheduler-rs schedules.
    if let Some(ref scheduler) = state.scheduler {
        if let Err(e) = rbh_policy::reconcile_triggers(scheduler, id, &body.definition).await {
            tracing::error!(policy_id = id, error = %e, "trigger reconciliation failed");
        }
    }
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

#[tracing::instrument(skip(state))]
async fn list_policies(State(state): State<AppState>) -> Result<Json<Vec<PolicyResponse>>, ApiError> {
    let rows = state.policy_store.list().await?;
    Ok(Json(rows.into_iter().map(PolicyResponse::from).collect()))
}

#[tracing::instrument(skip(state))]
async fn get_policy(State(state): State<AppState>, Path(id): Path<u64>) -> Result<Json<PolicyResponse>, ApiError> {
    let row = state.policy_store.get(id).await?;
    Ok(Json(PolicyResponse::from(row)))
}

#[tracing::instrument(skip(state, body))]
async fn update_policy(
    State(state): State<AppState>, Path(id): Path<u64>, Json(body): Json<CreatePolicyRequest>,
) -> Result<StatusCode, ApiError> {
    state.policy_store.update(id, &body.definition).await?;
    if let Some(ref scheduler) = state.scheduler {
        if let Err(e) = rbh_policy::reconcile_triggers(scheduler, id, &body.definition).await {
            tracing::error!(policy_id = id, error = %e, "trigger reconciliation failed");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[tracing::instrument(skip(state))]
async fn delete_policy(State(state): State<AppState>, Path(id): Path<u64>) -> Result<StatusCode, ApiError> {
    if let Some(ref scheduler) = state.scheduler {
        let _ = rbh_policy::reconcile::remove_policy_schedules(scheduler, id).await;
    }
    state.policy_store.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------

#[tracing::instrument(skip(state))]
async fn entry_count(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let count = state
        .entry_store
        .entry_count()
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "count": count })))
}

// ---- /api/entries/query ----

/// POST body for `/api/entries/query`.
#[derive(Debug, Deserialize)]
pub struct QueryRequest {
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
    pub fid: lustre_api::LuFid,
    pub parent_fid: Option<lustre_api::LuFid>,
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
    pub pool_name: Option<String>,
    pub sm_status: serde_json::Value,
    pub last_seen: i64,
}

impl From<EntryRow> for EntryDto {
    fn from(r: EntryRow) -> Self {
        Self {
            fid: r.fid,
            parent_fid: r.parent_fid,
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
    State(state): State<AppState>,
    Json(req): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, ApiError> {
    let limit = req.limit.min(MAX_LIMIT).max(1);

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
        .query_page(&where_clause, &store_params, order_by.as_deref(), limit, req.offset)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let total = if req.with_total {
        Some(
            state
                .entry_store
                .count_where(&where_clause, &store_params)
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?,
        )
    } else {
        None
    };

    Ok(Json(QueryResponse {
        entries: rows.into_iter().map(EntryDto::from).collect(),
        limit,
        offset: req.offset,
        total,
    }))
}

// ---- /api/reports/* ----

#[derive(Debug, Deserialize)]
pub struct AggregateRequest {
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
}

#[tracing::instrument(skip(state, req))]
async fn report_aggregate(
    State(state): State<AppState>,
    Json(req): Json<AggregateRequest>,
) -> Result<Json<Vec<AggregateRow>>, ApiError> {
    let sort = match req.sort {
        AggSort::Count => AggregateSort::Count,
        AggSort::Size => AggregateSort::Size,
    };
    let rows = state
        .entry_store
        .aggregate_by(req.key.as_column(), sort, req.limit.min(1_000).max(1))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        rows.into_iter()
            .map(|(key, count, total_size)| AggregateRow { key, count, total_size })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_top_n")]
    pub n: u64,
}

fn default_top_n() -> u64 {
    20
}

#[tracing::instrument(skip(state))]
async fn top_size(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<Vec<EntryDto>>, ApiError> {
    let rows = state
        .entry_store
        .query_page("1=1", &[], Some("size DESC"), q.n.min(1_000).max(1), 0)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(rows.into_iter().map(EntryDto::from).collect()))
}

#[tracing::instrument(skip(state))]
async fn oldest_entries(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListQuery>,
) -> Result<Json<Vec<EntryDto>>, ApiError> {
    let rows = state
        .entry_store
        .query_page("1=1", &[], Some("atime ASC"), q.n.min(1_000).max(1), 0)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(rows.into_iter().map(EntryDto::from).collect()))
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ApiError {
    Policy(PolicyError),
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
            ApiError::Policy(PolicyError::NotFoundByName(name)) => {
                (StatusCode::NOT_FOUND, format!("policy not found: name={name}"))
            }
            ApiError::Policy(PolicyError::InvalidTrigger(msg)) => (StatusCode::BAD_REQUEST, msg.clone()),
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
