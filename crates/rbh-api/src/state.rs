//! Shared application state for axum handlers.

use std::collections::HashMap;
use std::sync::Arc;

use rbh_entry_store::store::EntryStore;
use rbh_policy::{ClassifierRow, ClassifierStore, PolicyStore};
use scheduler_rs::prelude::Scheduler;
use tokio::sync::{Mutex, RwLock};

/// Shared state injected into all axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub policy_store: PolicyStore,
    pub classifier_store: ClassifierStore,
    /// Live classifier cache shared with the changelog ingest loop.
    /// Updated by create/update/delete handlers so incremental classification
    /// picks up changes without a daemon restart.
    pub classifier_cache: Arc<RwLock<Vec<ClassifierRow>>>,
    pub entry_store: EntryStore,
    pub scheduler: Option<Scheduler>,
    /// In-memory registry of scans started via POST /api/scans. Lives
    /// for the daemon process lifetime; cleared on restart. The mutex
    /// covers a small HashMap, contention is negligible.
    pub scans: Arc<Mutex<HashMap<String, ScanRecord>>>,
}

/// One fs-scan job started via POST /api/scans.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanRecord {
    pub id: String,
    pub root: String,
    pub since_mtime: Option<i64>,
    pub ignore_globs: Vec<String>,
    pub state: ScanState,
    pub scanned: u64,
    pub errors: u64,
    pub dirs: u64,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    Running,
    Completed,
    Failed,
}
