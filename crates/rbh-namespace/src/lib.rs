//! Backend-neutral, filesystem-scoped namespace resolution.

use std::path::PathBuf;

use rbh_entry_store::{EntryKey, EntryKind, FileSystemId};

/// Either side of a namespace lookup. Both variants resolve to the same
/// object/path/stat result so callers do not need backend-specific branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceTarget {
    Object(EntryKey),
    Path(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceStat {
    pub kind: EntryKind,
    pub size: u64,
    pub nlink: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNamespace {
    pub key: EntryKey,
    pub path: PathBuf,
    pub stat: NamespaceStat,
}

#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("object belongs to filesystem {actual}, not adapter {expected}")]
    WrongFilesystem {
        expected: FileSystemId,
        actual: FileSystemId,
    },
    #[error("path is outside filesystem {filesystem}: {path}")]
    OutsideFilesystem { filesystem: FileSystemId, path: PathBuf },
    #[error("object is not cataloged: {0:?}")]
    NotFound(EntryKey),
    #[error("namespace parent is missing for: {0:?}")]
    MissingParent(EntryKey),
    #[error("catalog path is stale for: {0:?}")]
    StalePath(EntryKey),
    #[error("namespace graph contains a cycle for: {0:?}")]
    Cycle(EntryKey),
    #[error("backend mismatch for filesystem {0}")]
    BackendMismatch(FileSystemId),
    #[error("namespace store failed: {0}")]
    Store(#[from] rbh_entry_store::StoreError),
    #[error("namespace I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Lustre namespace operation failed: {0}")]
    Lustre(#[from] lustre_api::LustreApiError),
    #[error("blocking namespace task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

mod adapter;
pub use adapter::NamespaceAdapter;
