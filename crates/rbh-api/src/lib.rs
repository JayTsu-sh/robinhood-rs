//! axum Router composing robinhood-specific REST endpoints.
//!
//! Endpoints:
//! * `POST /api/policies` — create a policy
//! * `GET /api/policies` — list policies, optionally with `?configuredOnly=true`
//! * `GET /api/policies/:id` — get policy by id
//! * `PUT /api/policies/:id` — update policy
//! * `POST /api/policies/preview` — evaluate an unsaved policy document
//! * `POST /api/policies/:id/preview` — evaluate policy candidates
//! * `DELETE /api/policies/:id` — delete policy
//! * `GET /api/runs` — search cross-policy execution history
//! * `GET /api/runs/{id}` — read a run and its pinned policy metadata
//! * `GET /api/runs/{id}/operations` — search a run's recorded file operations
//! * `GET /api/entries/count` — entry catalog count
//! * `GET /api/entries/{object_id}?filesystem=ID` — look up one catalog entry
//! * `POST /api/scans` — start a filesystem scan
//! * `GET /api/filesystems` — registered Lustre inventory on the daemon router
//! * `GET /api/filesystems/detail?filesystem=ID` — read one saved resource snapshot
//! * `POST /api/filesystems/refresh` — collect and save all or one registered Lustre resource
//!
//! Policy configuration writes use server-configured writer identities.

mod classifier_handlers;
mod console;
#[cfg(test)]
mod console_tests;
mod handlers;
mod policy_auth;
#[cfg(test)]
mod policy_execution_tests;
#[cfg(test)]
mod policy_preview_tests;
mod policy_resource_handlers;
#[cfg(test)]
mod read_heat_tests;
mod resource_management;
mod run_history;
#[cfg(test)]
mod run_history_tests;
pub mod state;

pub use policy_auth::PolicyWriters;
pub use resource_management::{ResourceAdministration, ResourceAdministrationError};
pub use state::AppState;

use axum::Router;
use rbh_lustre_resources::ResourceObservations;
use std::path::PathBuf;
use tower_http::trace::TraceLayer;

/// Build the application router with all endpoints.
pub fn router(state: AppState) -> Router {
    router_with_writers(state, PolicyWriters::default())
}

/// Build the REST API with an explicit policy configuration writer allowlist.
pub fn router_with_writers(state: AppState, writers: PolicyWriters) -> Router {
    Router::new()
        .nest("/api", handlers::api_routes().merge(policy_resource_handlers::routes()))
        .layer(axum::Extension(writers))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Build the daemon router with its registered resources and installed console assets.
pub fn router_with_console(
    state: AppState, writers: PolicyWriters, resources: ResourceObservations, assets_dir: PathBuf,
) -> Router {
    router_with_writers(state, writers).merge(console::router(resources, assets_dir))
}
