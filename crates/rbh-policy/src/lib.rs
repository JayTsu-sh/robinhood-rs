//! Policy engine — two-tier classification + action architecture.
//!
//! **Layer 1 — Classifiers** (`/api/classifiers`):
//! Write tag key-value pairs into `entries.sm_status.xattr.*` based on file
//! attributes. Driven by periodic scheduler-rs runs and Lustre changelog events.
//!
//! **Layer 2 — Action Policies** (`/api/policies`):
//! Filter entries by tags (`match_tags`) and execute a single action kind.
//! All attribute-based logic lives in classifiers; policies only reference tags.

pub mod classifier;
pub mod model;
pub mod ratelimit;
pub mod reconcile;
pub mod store;
pub mod task;
pub mod trigger_parser;

pub use classifier::{ClassifierDef, ClassifierRow, ClassifierRule, ClassifierStore, evaluate_classifier, parse_when};
pub use model::{
    ActionOpts, AlertParams, CmdParams, HsmParams, LruSortAttr, PolicyDef, PolicyKind, PolicyRow, RateLimit,
    RetryParams, TriggerSpec, WindowModeSpec,
};
pub use reconcile::reconcile_triggers;
pub use store::PolicyStore;
pub use task::TargetFilter;
pub use task::{PolicyRunTask, PolicyRuntime, init_runtime};
pub use trigger_parser::parse_trigger;

/// Errors produced by the policy layer.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("policy not found: id={0}")]
    NotFound(u64),
    #[error("policy not found: name={0}")]
    NotFoundByName(String),
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
    #[error("scheduler error: {0}")]
    Scheduler(String),
}
