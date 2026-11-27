use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use rbh_policy::{
    rev8::{
        ActionKind,
        resources::{ResourceKind, action_contract, contract_revision, original_archive_contract},
    },
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

fn contract_response(action: ActionKind, revision: u64, definition: Value) -> Value {
    let mut schema = serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false});
    match action {
        ActionKind::Archive => {
            schema["properties"]["archiveId"] =
                serde_json::json!({"type": "integer", "minimum": 0, "maximum": u32::MAX});
        }
        ActionKind::PoolMigrate => {
            let size = serde_json::json!({"oneOf": [
                {"type": "integer", "minimum": 1},
                {"type": "string", "pattern": "^[1-9][0-9]*(B|KiB|MiB|GiB|TiB|PiB)$"}
            ]});
            let pool = serde_json::json!({"type":"string","pattern":"^[a-zA-Z0-9_-]{1,15}$"});
            schema["required"] = serde_json::json!(["sourcePool", "targetPool", "maxFileBytes"]);
            schema["properties"] = serde_json::json!({
                "sourcePool": pool, "targetPool": pool, "maxFileBytes": size, "stripeSize": size,
                "stripeCount": {"type": "integer", "minimum": 1, "maximum": u16::MAX}
            });
            if revision >= 2 {
                schema["properties"]["minResidence"] =
                    serde_json::json!({"type": "string", "pattern": "^[1-9][0-9]*(ms|s|m|h|d)$"});
            }
        }
        _ => {}
    }
    serde_json::json!({
        "revision": revision,
        "definition": definition,
        "parametersSchema": schema
    })
}

async fn list_contracts() -> Json<Vec<Value>> {
    Json(ActionKind::ALL.into_iter().map(current_contract).collect())
}

fn current_contract(action: ActionKind) -> Value {
    contract_response(action, contract_revision(action), action_contract(action))
}

async fn get_contract(Path(name): Path<String>) -> Result<Json<Value>, ApiError> {
    ActionKind::from_name(&name)
        .map(current_contract)
        .map(Json)
        .ok_or_else(|| ApiError::NotFound(format!("unknown ActionContract/{name}")))
}

async fn get_contract_revision(Path((name, revision)): Path<(String, u64)>) -> Result<Json<Value>, ApiError> {
    match ActionKind::from_name(&name) {
        Some(ActionKind::Archive) if revision == 1 => Ok(Json(contract_response(
            ActionKind::Archive,
            1,
            original_archive_contract(),
        ))),
        Some(ActionKind::PoolMigrate) if revision == 1 => Ok(Json(contract_response(
            ActionKind::PoolMigrate,
            1,
            action_contract(ActionKind::PoolMigrate),
        ))),
        Some(action) if revision == contract_revision(action) => Ok(Json(current_contract(action))),
        _ => Err(ApiError::NotFound(format!(
            "unknown ActionContract/{name} revision {revision}"
        ))),
    }
}
