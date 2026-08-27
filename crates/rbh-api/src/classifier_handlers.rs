//! HTTP handlers for the `/api/classifiers` CRUD + run endpoints.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rbh_policy::{ClassifierDef, ClassifierRow, PolicyError, evaluate_classifier};
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

/// Reload the in-process classifier cache from the DB.
async fn reload_cache(state: &AppState) {
    match state.classifier_store.list().await {
        Ok(rows) => {
            let mut cache = state.classifier_cache.write().await;
            *cache = rows;
        }
        Err(e) => tracing::warn!(error = %e, "failed to reload classifier cache"),
    }
}

pub async fn create_classifier(
    State(state): State<AppState>, Json(body): Json<ClassifierDef>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let id = state.classifier_store.create(&body).await.map_err(policy_err)?;
    reload_cache(&state).await;
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
    reload_cache(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_classifier(
    State(state): State<AppState>, Path(id): Path<u64>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.classifier_store.delete(id).await.map_err(policy_err)?;
    reload_cache(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Manually run a classifier against all entries in the catalog.
///
/// Loads every filesystem-scoped catalog in pages and writes tags by scoped key.
pub async fn run_classifier(
    State(state): State<AppState>, Path(id): Path<u64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let row = state.classifier_store.get(id).await.map_err(policy_err)?;
    let def = &row.definition;

    if !def.enabled {
        return Ok(Json(serde_json::json!({"classified": 0, "skipped": "disabled"})));
    }

    const MAX_ENTRIES: u64 = 1_000_000;
    let mut classified: u64 = 0;
    let mut errors: u64 = 0;
    let mut scanned: u64 = 0;
    let filesystems = state
        .entry_store
        .list_filesystems()
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    for filesystem in filesystems {
        let mut offset = 0;
        let mut filesystem_scanned = 0;
        loop {
            if filesystem_scanned >= MAX_ENTRIES {
                tracing::warn!(
                    classifier_id = id,
                    filesystem = %filesystem.id,
                    scanned = filesystem_scanned,
                    max_entries = MAX_ENTRIES,
                    "per-filesystem max_entries limit reached"
                );
                break;
            }
            let limit = 10_000u64.min(MAX_ENTRIES - filesystem_scanned);
            let batch = state
                .entry_store
                .query_scoped_page(
                    &filesystem.id,
                    "1 = 1",
                    &[],
                    Some("object_kind, object_id"),
                    limit,
                    offset,
                )
                .await
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            if batch.is_empty() {
                break;
            }
            offset += batch.len() as u64;
            scanned += batch.len() as u64;
            filesystem_scanned += batch.len() as u64;
            for entry in &batch {
                if let Some(tags) = evaluate_classifier(def, entry) {
                    match state
                        .entry_store
                        .update_scoped_xattr(&entry.key, tags, &def.manages)
                        .await
                    {
                        Ok(true) => classified += 1,
                        Ok(false) => errors += 1,
                        Err(error) => {
                            errors += 1;
                            if errors <= 10 || errors.is_multiple_of(1000) {
                                tracing::warn!(filesystem = %filesystem.id, backend = ?filesystem.backend, object = ?entry.key.object(), %error, errors, "scoped xattr update failed during classifier run");
                            }
                        }
                    }
                }
            }
            if batch.len() < limit as usize {
                break;
            }
        }
    }

    tracing::info!(
        classifier_id = id,
        classifier_name = %def.name,
        classified,
        errors,
        scanned,
        "manual classifier run complete"
    );
    Ok(Json(
        serde_json::json!({ "classified": classified, "errors": errors, "scanned": scanned }),
    ))
}
