//! MariaDB-backed entry catalog for robinhood-rs.
//!
//! Owns the `rbh_entries` database. Tables:
//! * `entries` — FID-keyed catalog of Lustre files/dirs (Phase 2)
//! * `names` — hardlink support (Phase 2)
//! * `removed_entries` — deleted files for HSM cleanup (Phase 2)
//! * `changelog_cursor` — per-MDT cursor for `CursorStore` (Phase 2)
//! * `filesystems` / `scoped_entries` — additive filesystem-scoped identity
//!   used while legacy FID-keyed callers are migrated
//!
//! Policy tables (`policies`, `policy_trigger_state`, etc.) are added in Phase 6+.
//!
//! See `.claude/memory/persistence_boundary.md`: this crate owns `rbh_entries`
//! exclusively; it never writes to `rbh_scheduler` (owned by scheduler-rs).

pub mod error;
pub mod fid_codec;
pub mod model;
pub mod store;

pub use error::StoreError;
pub use model::{
    BackendCapabilities, BackendKind, BackendKindParseError, EntryKey, EntryKind, EntryRow, FileSystemConfig,
    FileSystemId, FileSystemIdError, ObjectId, RemovedEntry, ScopedEntryRow, ScopedRemovedEntry,
};
pub use store::EntryStore;
