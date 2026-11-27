//! Policy engine — two-tier classification + action architecture.
//!
//! **Layer 1 — Classifiers** (`/api/classifiers`):
//! Write tag key-value pairs into `entries.sm_status.xattr.*` based on file
//! attributes. Driven by periodic scheduler-rs runs and Lustre changelog events.
//!
//! **Layer 2 — Action Policies** (`/api/policies`):
//! REST-managed Rev.8 policies select candidates and dispatch native actions.

pub mod classifier;
pub mod model;
pub mod rev8;
pub mod store;
pub mod task;
pub mod trigger_parser;

pub use classifier::{ClassifierDef, ClassifierRow, ClassifierRule, ClassifierStore, evaluate_classifier, parse_when};
pub use model::TriggerSpec;
pub use rbh_entry_store::FileSystemId;
pub use store::PolicyStore;
pub use task::{PolicyRuntime, Rev8TickTask, init_runtime};
pub use trigger_parser::parse_trigger;

/// Errors produced by the policy layer.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy not found: id={0}")]
    NotFound(u64),
    #[error("policy name already exists: {0}")]
    NameConflict(String),
    #[error("resource not found: {0}")]
    ResourceNotFound(String),
    #[error("{0}")]
    ResourceInUse(String),
    #[error("{0}")]
    InvalidPolicy(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migration(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("store error: {0}")]
    Store(String),
    #[error("invalid trigger: {0}")]
    InvalidTrigger(String),
}
