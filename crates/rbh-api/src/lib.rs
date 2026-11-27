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
//! No auth layer (trusted microservice). Uses `tower-http::TraceLayer` for
//! per-request spans with method/path/status/latency.

mod classifier_handlers;
mod handlers;
pub mod state;

pub use state::AppState;

use axum::Router;
use tower_http::trace::TraceLayer;

/// Build the application router with all endpoints.
pub fn router(state: AppState) -> Router {
    Router::new()
        .nest("/api", handlers::api_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
