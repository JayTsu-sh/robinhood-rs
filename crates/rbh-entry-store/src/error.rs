//! Error types for the entry store.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("entry not found: fid={0}")]
    NotFound(lustre_api::LuFid),

    #[error("FID encoding error: {0}")]
    FidCodec(&'static str),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid persisted backend kind: {0}")]
    InvalidBackendKind(#[from] crate::model::BackendKindParseError),

    #[error("invalid persisted filesystem id: {0}")]
    InvalidFileSystemId(#[from] crate::model::FileSystemIdError),

    #[error("invalid persisted object identity: {0}")]
    InvalidObjectIdentity(&'static str),

    #[error("filesystem {0} is not registered")]
    UnknownFilesystem(crate::model::FileSystemId),

    #[error("filesystem {filesystem} uses backend {configured:?}, but object id uses {object:?}")]
    BackendMismatch {
        filesystem: crate::model::FileSystemId,
        configured: crate::model::BackendKind,
        object: crate::model::BackendKind,
    },
}

pub type Result<T> = std::result::Result<T, StoreError>;
