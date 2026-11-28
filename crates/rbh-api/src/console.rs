//! Same-origin console assets and Lustre resource snapshot endpoints.

use std::{convert::Infallible, path::PathBuf};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::{StreamExt, future, stream};
use rbh_entry_store::FileSystemId;
use rbh_lustre_resources::{ResourceObservations, ResourceUpdate};
use serde::Deserialize;
use tower_http::{services::ServeDir, trace::TraceLayer};

use crate::handlers::ApiError;

pub(crate) fn router(resources: ResourceObservations, assets_dir: PathBuf) -> Router {
    let resources = Router::new()
        .route("/api/filesystems", get(list_filesystems))
        .route("/api/filesystems/detail", get(get_filesystem))
        .route("/api/filesystems/events", get(resource_events))
        .route("/api/filesystems/refresh", post(refresh_filesystems))
        .with_state(resources);
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

#[tracing::instrument(skip_all)]
async fn resource_events(State(resources): State<ResourceObservations>) -> impl IntoResponse {
    let updates = resources.updates().map(|update| match update {
        ResourceUpdate::Snapshot(id) => Event::default().event("snapshot").data(id.as_str()),
        ResourceUpdate::Management(id) => Event::default()
            .event("management")
            .data(id.as_ref().map_or("*", FileSystemId::as_str)),
        ResourceUpdate::Resync => Event::default().event("resync").data("*"),
    });
    let events = stream::once(future::ready(Event::default().comment("connected")))
        .chain(updates)
        .map(Ok::<_, Infallible>);
    (
        [("x-accel-buffering", "no")],
        Sse::new(events).keep_alive(KeepAlive::default()),
    )
}

#[tracing::instrument(skip_all)]
async fn list_filesystems(State(resources): State<ResourceObservations>) -> Result<Response, ApiError> {
    let rows = resources.list().await.map_err(snapshot_error)?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(rows)).into_response())
}

#[derive(Debug, Deserialize)]
struct FilesystemQuery {
    filesystem: FileSystemId,
}

#[tracing::instrument(skip(resources), fields(filesystem = %query.filesystem))]
async fn get_filesystem(
    State(resources): State<ResourceObservations>, Query(query): Query<FilesystemQuery>,
) -> Result<Response, ApiError> {
    let resource = resources
        .get(&query.filesystem)
        .await
        .map_err(snapshot_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Lustre 文件系统 {} 未注册", query.filesystem)))?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(resource)).into_response())
}

#[derive(Debug, Deserialize)]
struct RefreshQuery {
    filesystem: Option<FileSystemId>,
}

#[tracing::instrument(skip(resources))]
async fn refresh_filesystems(
    State(resources): State<ResourceObservations>, Query(query): Query<RefreshQuery>,
) -> Result<StatusCode, ApiError> {
    if let Some(id) = &query.filesystem
        && !resources.contains(id)
    {
        return Err(ApiError::NotFound(format!("Lustre 文件系统 {id} 未注册")));
    }
    resources.refresh(query.filesystem.as_ref()).await.map_err(|reason| {
        tracing::warn!(%reason, filesystem = ?query.filesystem, "resource refresh failed");
        ApiError::ServiceUnavailable("资源刷新失败，请重试".to_owned())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

fn snapshot_error(error: sqlx::Error) -> ApiError {
    ApiError::Internal(format!("load resource snapshots: {error}"))
}
