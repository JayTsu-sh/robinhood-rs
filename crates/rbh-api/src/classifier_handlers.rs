//! HTTP handlers for the `/api/classifiers` CRUD endpoints.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rbh_policy::{ClassifierDef, ClassifierRow, PolicyError};
use serde::Serialize;

use crate::AppState;

/// Response shape for a classifier row.
#[derive(Debug, Serialize)]
pub struct ClassifierResponse {
    pub id: u64,
    pub name: String,
    pub definition: ClassifierDef,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<ClassifierRow> for ClassifierResponse {
    fn from(r: ClassifierRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            enabled: r.enabled,
            created_at: r.created_at.to_rfc3339(),
            updated_at: r.updated_at.to_rfc3339(),
            definition: r.definition,
        }
    }
}

fn policy_err(e: PolicyError) -> (StatusCode, String) {
    match &e {
        PolicyError::NotFound(_) | PolicyError::NotFoundByName(_) => (StatusCode::NOT_FOUND, e.to_string()),
        PolicyError::InvalidTrigger(_) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

pub async fn create_classifier(
    State(state): State<AppState>, Json(body): Json<ClassifierDef>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let id = state.classifier_store.create(&body).await.map_err(policy_err)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

pub async fn list_classifiers(
    State(state): State<AppState>,
) -> Result<Json<Vec<ClassifierResponse>>, (StatusCode, String)> {
    let rows = state.classifier_store.list().await.map_err(policy_err)?;
    Ok(Json(rows.into_iter().map(ClassifierResponse::from).collect()))
}

pub async fn get_classifier(
    State(state): State<AppState>, Path(id): Path<u64>,
) -> Result<Json<ClassifierResponse>, (StatusCode, String)> {
    let row = state.classifier_store.get(id).await.map_err(policy_err)?;
    Ok(Json(ClassifierResponse::from(row)))
}

pub async fn update_classifier(
    State(state): State<AppState>, Path(id): Path<u64>, Json(body): Json<ClassifierDef>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.classifier_store.update(id, &body).await.map_err(policy_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_classifier(
    State(state): State<AppState>, Path(id): Path<u64>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.classifier_store.delete(id).await.map_err(policy_err)?;
    Ok(StatusCode::NO_CONTENT)
}
