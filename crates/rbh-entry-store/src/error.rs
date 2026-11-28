//! Error types for the entry store.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("database schema is missing or incomplete. Initialize an empty database with rbh-entry-store/schema.sql")]
    SchemaMissing,

    #[error(
        "database schema version {actual} is unsupported (expected {expected}). Initialize an empty database with rbh-entry-store/schema.sql"
    )]
    SchemaVersion { actual: u32, expected: u32 },

    #[error(
        "database schema is incomplete: missing InnoDB table {0}. Initialize an empty database with rbh-entry-store/schema.sql"
    )]
    SchemaTableMissing(&'static str),

    #[error(
        "database schema is incomplete: missing policy lock row in {0}. Initialize an empty database with rbh-entry-store/schema.sql"
    )]
    SchemaLockMissing(&'static str),

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

    #[error("layout value out of range: {0}")]
    InvalidLayout(&'static str),

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
