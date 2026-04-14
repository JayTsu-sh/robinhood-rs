//! Action executors for robinhood-rs policies.
//!
//! Each [`PolicyKind`] maps to a concrete [`ActionExecutor`] implementation:
//! * `Purge` → [`PurgeExecutor`] (unlink files, rmdir empty dirs)
//! * `HsmArchive` → [`HsmArchiveExecutor`] (submit HSM archive request)
//! * `HsmRelease` → [`HsmReleaseExecutor`] (submit HSM release request)
//! * `Migration` / `Alert` → stubs for later phases
//!
//! The [`dispatch`] function selects the right executor based on policy kind.

mod executor;

pub use executor::{
    ActionContext, ActionExecutor, ActionOutcome, HsmArchiveExecutor, HsmReleaseExecutor,
    PurgeExecutor,
};

/// Errors produced by action executors.
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("lustre HSM error: {0}")]
    Hsm(#[from] lustre_api::error::LustreApiError),
    #[error("entry store error: {0}")]
    Store(String),
    #[error("action not implemented: {0}")]
    NotImplemented(String),
    #[error("entry has no path (parent_fid or name missing)")]
    NoPath,
}
