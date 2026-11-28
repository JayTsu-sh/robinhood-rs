//! MariaDB-backed entry catalog for robinhood-rs.
//!
//! The project-wide MariaDB schema is installed from `schema.sql`.
//! This crate validates it on connection and owns the filesystem-scoped
//! catalog, changelog cursors, and the Lustre compatibility catalog.

pub mod error;
pub mod fid_codec;
pub mod model;
mod schema;
pub mod store;

pub use error::StoreError;
pub use model::{
    BackendCapabilities, BackendKind, BackendKindParseError, EntryKey, EntryKind, EntryRow, FileSystemConfig,
    FileSystemId, FileSystemIdError, ObjectId, RemovedEntry, ScopedEntryRow, ScopedRemovedEntry,
};
pub use store::EntryStore;
