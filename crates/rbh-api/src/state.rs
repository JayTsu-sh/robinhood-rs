//! Shared application state for axum handlers.

use rbh_entry_store::store::EntryStore;
use rbh_policy::PolicyStore;
use scheduler_rs::prelude::Scheduler;

/// Shared state injected into all axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub policy_store: PolicyStore,
    pub entry_store: EntryStore,
    pub scheduler: Option<Scheduler>,
}
