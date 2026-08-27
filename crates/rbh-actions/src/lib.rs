//! Action executors for robinhood-rs policies.
//!
//! Each [`PolicyKind`] maps to a concrete [`ActionExecutor`] implementation:
//! * `Purge`      → [`PurgeExecutor`]         (unlink files, rmdir empty dirs)
//! * `HsmArchive` → [`HsmArchiveExecutor`]    (submit HSM archive request)
//! * `HsmRelease` → [`HsmReleaseExecutor`]    (submit HSM release request)
//! * `HsmRestore` → [`HsmRestoreExecutor`]    (restore released file from HSM)
//! * `HsmRemove`  → [`HsmRemoveExecutor`]     (remove HSM backend copy)
//! * `Migration`  → [`CmdExecutor`]           (arbitrary shell command, e.g. lfs migrate)
//! * `Alert`      → [`AlertExecutor`]         (webhook / tracing log)
//! * `Backup`     → [`BackupExecutor`]        (external rbhext_tool protocol)

mod backend;
mod executor;

pub use backend::{ActionBackend, BackendAction, BackendActionOutcome};

pub use executor::{
    ActionContext, ActionExecutor, ActionOutcome, AlertExecutor, BackupExecutor, CmdExecutor, HsmArchiveExecutor,
    HsmReleaseExecutor, HsmRemoveExecutor, HsmRestoreExecutor, PurgeExecutor,
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
    #[error("external backup tool error: {0}")]
    Backup(#[from] rbh_backup::BackupError),
    #[error("action not implemented: {0}")]
    NotImplemented(String),
    #[error("entry has no path (parent_fid or name missing)")]
    NoPath,
    #[error("backend capability error: {0}")]
    Capability(String),
    #[error("namespace action failed: {0}")]
    Namespace(#[from] rbh_namespace::NamespaceError),
}
