//! Async Lustre changelog listener built on top of `lustre-api`.
//!
//! Phase 1b produces the domain layer above the raw FFI in `lustre-api`:
//!
//! * [`event::ChangelogEvent`] — `#[serde(tag = "kind")]` enum, one variant per
//!   record kind, with fully-expanded per-variant fields.
//! * [`event::ChangelogEventEnvelope`] — wraps an event with stream metadata
//!   (mdt, record index, wall-clock time).
//! * [`parse::parse_event`] — converts a [`lustre_api::RecView`] into a
//!   [`ChangelogEvent`] via the [`parse::RawRec`] abstraction.
//! * [`dedup::Deduplicator`] — applies robinhood-C's per-FID dedup rules
//!   (`IGNORE_ALWAYS`, `IGNORE_MASK`, `IGNORE_CANCEL`) before records leave the
//!   listener.
//! * [`batcher::EventBatcher`] — combines dedup with size / time flush
//!   triggers and a global pending soft cap.
//! * [`cursor::CursorStore`] — trait for persisting the last-committed record
//!   index per MDT. A [`cursor::MemoryCursorStore`] is provided for tests;
//!   `rbh-entry-store` implements a MariaDB-backed variant in Phase 2.
//! * [`listener::ChangelogListener`] — ties it all together: runs the sync
//!   `llapi_changelog_recv` loop on a [`tokio::task::spawn_blocking`] thread,
//!   emits [`listener::EventBatch`]es through a bounded mpsc channel, and
//!   advances the Lustre-side clear watermark in response to
//!   [`listener::EventAck`]s sent back by the consumer.
//!
//! See `.claude/memory/phase1b_decisions.md` for the full scope and rationale.

// Phase 1b lands in stages (see phase1b_decisions.md). Private helpers and
// types added early for later stages' consumption generate dead-code warnings
// until Stage F wires everything together. Remove this allow once the
// listener module is complete.
#![allow(dead_code)]

pub mod batcher;
pub mod cursor;
pub mod dedup;
pub mod error;
pub mod event;
pub mod listener;
pub mod parse;

pub use batcher::{BatcherConfig, EventBatch};
pub use cursor::{CursorError, CursorStore, MemoryCursorStore};
pub use error::ChangelogError;
pub use event::{ChangelogEvent, ChangelogEventEnvelope};
pub use listener::{ChangelogListener, EventAck, ListenerConfig, ListenerHandle};
pub use parse::{FakeRec, RawRec};
