//! HTTP request handlers for the robinhood-rs REST API.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use rbh_policy::{PolicyDef, PolicyError};

use crate::AppState;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/policies", post(create_policy).get(list_policies))
        .route(
            "/policies/{id}",
            get(get_policy).put(update_policy).delete(delete_policy),
        )
        .route("/entries/count", get(entry_count))
        .route("/health", get(health))
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
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
