//! Policy engine — bridges predicates to scheduler-rs for timed execution.
//!
//! A [`PolicyDef`] is a declarative struct (stored as JSON in MariaDB) that
//! combines a scope predicate, first-match rules, action parameters, and
//! trigger specifications. Each trigger maps 1:1 to a scheduler-rs schedule;
//! [`reconcile_triggers`] handles the create/delete lifecycle.
//!
//! At fire time, [`PolicyRunTask`] loads the definition, queries candidates
//! from `rbh-entry-store` via SQL pushdown, evaluates rules in-memory, and
//! dispatches matched entries to action executors (stubbed until `rbh-actions`).

pub mod model;
pub mod reconcile;
pub mod store;
pub mod task;

pub use model::{ActionParams, PolicyDef, PolicyKind, PolicyRow, Rule, TriggerSpec, WindowModeSpec};
pub use reconcile::reconcile_triggers;
pub use store::PolicyStore;
pub use task::{PolicyRunTask, PolicyRuntime, init_runtime};

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
    #[error("invalid trigger: {0}")]
    InvalidTrigger(String),
    #[error("scheduler error: {0}")]
    Scheduler(String),
}
