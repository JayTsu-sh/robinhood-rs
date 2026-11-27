use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use rbh_policy::{
    rev8::resources::{ARCHIVE, ARCHIVE_REVISION, ResourceKind, archive_contract},
    store::ResourceRevision,
};
use serde_json::Value;

use crate::{AppState, Rev8Writers, handlers::ApiError};

pub(crate) fn routes() -> Router<AppState> {
    [
        ("/selectors", ResourceKind::Selector),
        ("/execution-profiles", ResourceKind::ExecutionProfile),
        ("/authorization-policies", ResourceKind::AuthorizationPolicy),
        ("/approval-policies", ResourceKind::ApprovalPolicy),
        ("/system-safety-policies", ResourceKind::SystemSafetyPolicy),
    ]
    .into_iter()
    .fold(
        Router::new()
            .route("/action-contracts", get(list_contracts))
            .route("/action-contracts/{name}", get(get_contract))
            .route(
                "/action-contracts/{name}/revisions/{revision}",
                get(get_contract_revision),
            ),
        |router, (path, kind)| {
            router.merge(
                Router::new()
                    .route(path, post(create_resource).get(list_resources))
                    .route(
                        &format!("{path}/{{name}}"),
                        get(get_resource).put(update_resource).delete(delete_resource),
                    )
                    .route(&format!("{path}/{{name}}/revisions/{{revision}}"), get(get_revision))
                    .layer(Extension(kind)),
            )
        },
    )
}

async fn list_resources(
    State(state): State<AppState>, Extension(kind): Extension<ResourceKind>,
) -> Result<Json<Vec<ResourceRevision>>, ApiError> {
    Ok(Json(state.policy_store.list_resources(kind).await?))
}

async fn get_resource(
    State(state): State<AppState>, Extension(kind): Extension<ResourceKind>, Path(name): Path<String>,
) -> Result<Json<ResourceRevision>, ApiError> {
    Ok(Json(state.policy_store.get_resource(kind, &name, None).await?))
}

async fn get_revision(
    State(state): State<AppState>, Extension(kind): Extension<ResourceKind>,
    Path((name, revision)): Path<(String, u64)>,
) -> Result<Json<ResourceRevision>, ApiError> {
    Ok(Json(
        state.policy_store.get_resource(kind, &name, Some(revision)).await?,
    ))
}

async fn create_resource(
    State(state): State<AppState>, Extension(kind): Extension<ResourceKind>,
    Extension(writers): Extension<Rev8Writers>, headers: HeaderMap, Json(document): Json<Value>,
) -> Result<(StatusCode, Json<ResourceRevision>), ApiError> {
    let actor = writers.authorize(&headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.policy_store.save_resource(kind, None, document, actor).await?),
    ))
}

async fn update_resource(
    State(state): State<AppState>, Extension(kind): Extension<ResourceKind>, Path(name): Path<String>,
    Extension(writers): Extension<Rev8Writers>, headers: HeaderMap, Json(document): Json<Value>,
) -> Result<Json<ResourceRevision>, ApiError> {
    let actor = writers.authorize(&headers)?;
    Ok(Json(
        state
            .policy_store
            .save_resource(kind, Some(&name), document, actor)
            .await?,
    ))
}

async fn delete_resource(
    State(state): State<AppState>, Extension(kind): Extension<ResourceKind>, Path(name): Path<String>,
    Extension(writers): Extension<Rev8Writers>, headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let actor = writers.authorize(&headers)?;
    state.policy_store.delete_resource(kind, &name, actor).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn archive_revision() -> Value {
    serde_json::json!({
        "revision": 1,
        "definition": archive_contract(),
        "parametersSchema": {
            "type": "object",
            "properties": {"archiveId": {"type": "integer", "minimum": 0, "maximum": u32::MAX}},
            "additionalProperties": false
        }
    })
}

async fn list_contracts() -> Json<Vec<Value>> {
    Json(vec![archive_revision()])
}

async fn get_contract(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    contract_revision(&name, ARCHIVE_REVISION)
}

async fn get_contract_revision(Path((name, revision)): Path<(String, u64)>) -> Result<Json<Value>, ApiError> {
    contract_revision(&name, revision)
}

fn contract_revision(name: &str, revision: u64) -> Result<Json<Value>, ApiError> {
    match (name, revision) {
        (ARCHIVE, 1) => Ok(Json(archive_revision())),
        _ => Err(ApiError::NotFound(format!(
            "unknown ActionContract/{name} revision {revision}"
        ))),
    }
}
