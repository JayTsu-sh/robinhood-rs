use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use lustre_api::LuFid;
use rbh_policy::store::{
    HistoryOperation, HistoryPage, OperationSearch, OperationState, RunHistory, RunSearch, RunState,
};
use serde::Deserialize;

use crate::{AppState, handlers::ApiError};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/runs", get(list_runs))
        .route("/runs/{id}", get(get_run))
        .route("/runs/{id}/operations", get(list_operations))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunQuery {
    policy_id: Option<u64>,
    policy_name: Option<String>,
    run_id: Option<u64>,
    state: Option<RunState>,
    before_id: Option<u64>,
    limit: Option<u16>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationQuery {
    state: Option<OperationState>,
    reason: Option<String>,
    fid: Option<String>,
    #[serde(default)]
    exceptions_only: bool,
    after_id: Option<u64>,
    limit: Option<u16>,
}

fn page_limit(limit: Option<u16>) -> Result<u16, ApiError> {
    let limit = limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return Err(ApiError::BadRequest("limit must be between 1 and 100".into()));
    }
    Ok(limit)
}

fn trimmed(value: Option<String>) -> Option<String> {
    value.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
}

#[tracing::instrument(skip(state))]
async fn list_runs(
    State(state): State<AppState>, Query(query): Query<RunQuery>,
) -> Result<Json<HistoryPage<RunHistory>>, ApiError> {
    let filter = RunSearch {
        policy_id: query.policy_id,
        policy_name: trimmed(query.policy_name),
        run_id: query.run_id,
        state: query.state,
        before_id: query.before_id,
        limit: page_limit(query.limit)?,
    };
    Ok(Json(state.policy_store.search_rev8_runs(filter).await?))
}

#[tracing::instrument(skip(state))]
async fn get_run(State(state): State<AppState>, Path(id): Path<u64>) -> Result<Json<RunHistory>, ApiError> {
    Ok(Json(
        state
            .policy_store
            .rev8_run_detail(id)
            .await?
            .ok_or_else(|| run_not_found(id))?,
    ))
}

#[tracing::instrument(skip(state))]
async fn list_operations(
    State(state): State<AppState>, Path(id): Path<u64>, Query(query): Query<OperationQuery>,
) -> Result<Json<HistoryPage<HistoryOperation>>, ApiError> {
    let filter = OperationSearch {
        state: query.state,
        reason: query.reason.filter(|reason| !reason.is_empty()),
        exceptions_only: query.exceptions_only,
        fid: trimmed(query.fid)
            .map(|value| value.parse::<LuFid>())
            .transpose()
            .map_err(|error| ApiError::BadRequest(error.to_string()))?,
        after_id: query.after_id,
        limit: page_limit(query.limit)?,
    };
    Ok(Json(
        state
            .policy_store
            .search_rev8_operations(id, filter)
            .await?
            .ok_or_else(|| run_not_found(id))?,
    ))
}

fn run_not_found(id: u64) -> ApiError {
    ApiError::NotFound(format!("run not found: id={id}"))
}
