//! Async Lustre filesystem walker for initial catalog seeding.
//!
//! [`FsScanner::run`] walks a Lustre mount, stats each entry, resolves FIDs
//! and stripe info via `lustre-api`, and emits [`ScanEvent`]s through a
//! bounded channel. The caller (typically `rbh-daemon`) drains events and
//! upserts them into `rbh-entry-store`.
//!
//! Concurrency uses a shared `async_channel` work queue with an `AtomicUsize`
//! pending counter for correct termination (see `memory/fs_scan_reuses_terrasync_walker.md`).

mod entry;
mod posix;
mod walker;

pub use entry::{build_entry, enrich_lustre};
pub use posix::{PosixEntry, PosixWalkEvent, PosixWalker};
pub use walker::{FsScanner, ScanConfig, ScanEvent, ScanProgress, load_rbh_ignore_file};

/// Errors produced by the filesystem scanner.
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("IO error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("Lustre API error: {0}")]
    Lustre(#[from] lustre_api::error::LustreApiError),
    #[error("walker channel closed")]
    ChannelClosed,
}
