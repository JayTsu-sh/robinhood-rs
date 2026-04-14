//! Safe wrapper around `liblustreapi` for robinhood-rs.
//!
//! Exposes changelog, FID/path, stripe info, and HSM operations. The API is
//! synchronous because liblustreapi itself is blocking — async callers should use
//! `tokio::task::spawn_blocking` at the call site.
//!
//! # Versioning
//!
//! Targets Lustre **2.12 or later**. Runtime feature detection for LU-543, LU-1331,
//! and flex-format changelog records is NOT performed; those features are assumed
//! present. See `.claude/memory/phase1a_decisions.md` for the full scope.
//!
//! # Module layout
//!
//! * [`error`] — `LustreApiError`, `HsmOp`, and the `Result` alias.
//! * [`fid`] — `LuFid` with `Display` / `FromStr` / serde support.
//! * `rec`, `changelog`, `mdt`, `path`, `stripe`, `hsm` — to be filled in during
//!   Phase 1a stages B–D.
//!
//! The public entry point is the zero-sized [`LustreApi`] struct; each functional
//! area exposes methods on it from a dedicated module.

// Phase 1a is implemented in stages; the helpers in `sys`, `error`, and `fid`
// that aren't wired up yet (HSM constants, `check_rc_hsm`, `LuFid::{from_raw,
// into_raw}`, etc.) are consumed by the functional modules added in Stages B–D.
// Remove this allow once Stage D lands and the whole crate is in use.
#![allow(dead_code)]

mod sys;

pub mod changelog;
pub mod error;
pub mod fid;
pub mod hsm;
pub mod mdt;
pub mod path;
pub mod rec;
pub mod stripe;

pub use changelog::{ChangelogHandle, RecvBuf};
pub use error::{HsmOp, LustreApiError, Result};
pub use fid::{FidParseError, LuFid};
pub use hsm::{HsmAction, HsmRequestBuilder, HsmRequestFlags, HsmState, HsmUserStateInfo};
pub use mdt::ChangelogUser;
pub use rec::{ChangelogEventType, RecView};
pub use stripe::StripeInfo;

/// Zero-sized handle to `liblustreapi`.
///
/// Cheap to construct and clone. Methods are sync because the underlying FFI is
/// blocking; async callers wrap individual calls in `tokio::task::spawn_blocking`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LustreApi;

impl LustreApi {
    /// Create a new handle. Equivalent to `LustreApi::default()`.
    pub const fn new() -> Self {
        Self
    }
}
