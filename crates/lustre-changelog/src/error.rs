//! Error types for the `lustre-changelog` crate.
//!
//! Library-layer errors use `thiserror` with `#[from]` conversions per the
//! `rust_design_style.md` rule 10. Binary callers wrap in `anyhow`.

use crate::cursor::CursorError;

/// All fallible operations in `lustre-changelog` return this error type.
#[derive(Debug, thiserror::Error)]
pub enum ChangelogError {
    /// The underlying liblustreapi call failed.
    #[error("lustre-api error")]
    LustreApi(#[from] lustre_api::LustreApiError),

    /// A changelog record could not be parsed into a domain event.
    ///
    /// `reason` carries a short, operator-actionable description (e.g.
    /// "unknown record type 42", "name is not NUL-terminated").
    #[error("failed to parse changelog record at index {index}: {reason}")]
    Parse { index: u64, reason: &'static str },

    /// The consumer-side ack channel closed before we finished receiving acks.
    ///
    /// When this happens the listener stops advancing its clear watermark and
    /// shuts down; remaining records stay on the MDT until a fresh listener
    /// starts.
    #[error("ack channel closed before listener shutdown")]
    AckChannelClosed,

    /// The event-output channel closed — the consumer dropped its receiver.
    #[error("event channel closed; consumer is gone")]
    EventChannelClosed,

    /// The CursorStore returned an error while reading or committing.
    #[error("cursor store error")]
    Cursor(#[from] CursorError),

    /// The blocking thread joined with a panic.
    #[error("listener blocking task panicked")]
    BlockingJoin,
}
