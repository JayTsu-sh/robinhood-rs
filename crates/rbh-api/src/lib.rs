//! axum Router composing robinhood-specific REST endpoints.
//!
//! Endpoints:
//! * `POST /api/policies` — create a policy
//! * `GET /api/policies` — list all policies
//! * `GET /api/policies/:id` — get policy by id
//! * `PUT /api/policies/:id` — update policy
//! * `POST /api/policies/:id/preview` — evaluate Rev.8 candidates
//! * `DELETE /api/policies/:id` — delete policy
//! * `GET /api/entries/count` — entry catalog count
//! * `GET /api/entries/{object_id}?filesystem=ID` — look up one catalog entry
//! * `POST /api/scans` — start a filesystem scan
//!
//! Rev.8 configuration writes use server-configured writer identities.

mod classifier_handlers;
mod handlers;
mod rev8_auth;
#[cfg(test)]
mod rev8_execution_tests;
mod rev8_handlers;
pub mod state;

pub use rev8_auth::Rev8Writers;
pub use state::AppState;

use axum::Router;
use tower_http::trace::TraceLayer;

/// Build the application router with all endpoints.
pub fn router(state: AppState) -> Router {
    router_with_writers(state, Rev8Writers::default())
}

/// Build the REST API with an explicit Rev.8 configuration writer allowlist.
pub fn router_with_writers(state: AppState, writers: Rev8Writers) -> Router {
    Router::new()
        .nest("/api", handlers::api_routes().merge(rev8_handlers::routes()))
        .layer(axum::Extension(writers))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
