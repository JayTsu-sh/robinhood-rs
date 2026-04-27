//! Changelog stream lifecycle — `open` / `recv` / `clear` / `close`.
//!
//! Thin safe wrappers over `llapi_changelog_{start,recv,free,fini,clear}` and
//! the RAII types that track record and handle ownership.
//!
//! The methods are synchronous; async callers wrap in `tokio::task::spawn_blocking`.
//! The typical usage pattern for `lustre-changelog`'s listener is:
//!
//! ```ignore
//! let api = LustreApi::new();
//! let mut handle = api.open_changelog("testfs-MDT0000", 0, /* follow= */ true)?;
//! loop {
//!     match api.recv_changelog(&mut handle)? {
//!         Some(rec) => {
//!             let view = unsafe { RecView::new(rec.as_ptr()) };
//!             // process view ...
//!             drop(rec); // calls llapi_changelog_free
//!         }
//!         None => break, // EOF
//!     }
//! }
//! api.clear_changelog("testfs-MDT0000", "cl1", last_processed_rec)?;
//! ```

use core::ffi::c_void;
use core::ptr;
use std::ffi::CString;

use tracing::{debug, warn};

use crate::LustreApi;
use crate::error::{Result, check_rc};
use crate::sys;

/// RAII wrapper over an opaque `void*` changelog stream.
///
/// Dropping a `ChangelogHandle` calls `llapi_changelog_fini` best-effort and
/// logs any error via `tracing::warn!`. For explicit error handling, call
/// [`LustreApi::close_changelog`] before the handle is dropped.
///
/// The handle is `Send` but NOT `Sync`: the liblustreapi state behind it is
/// single-consumer and cannot be shared across threads simultaneously.
pub struct ChangelogHandle {
    priv_: *mut c_void,
    mdt: String,
}

impl ChangelogHandle {
    /// Raw handle pointer, for internal use by [`LustreApi::recv_changelog`].
    #[inline]
    fn raw(&self) -> *mut c_void {
        self.priv_
    }

    /// Name of the MDT this handle is bound to. Useful for logging.
    pub fn mdt(&self) -> &str {
        &self.mdt
    }
}

// SAFETY: liblustreapi's changelog handle state is thread-safe for Move
// (transferring ownership across threads) but not for concurrent access.
// `!Sync` is enforced by the lack of a Sync impl.
unsafe impl Send for ChangelogHandle {}

impl Drop for ChangelogHandle {
    fn drop(&mut self) {
        if self.priv_.is_null() {
            return;
        }
        // SAFETY: `priv_` is the pointer returned by `llapi_changelog_start`
        // and nothing else has closed it (we're the only owner).
        let rc = unsafe { sys::llapi_changelog_fini(&mut self.priv_) };
        if rc < 0 {
            warn!(
                mdt = %self.mdt,
                errno = -rc,
                "llapi_changelog_fini failed in ChangelogHandle::drop"
            );
        }
        self.priv_ = ptr::null_mut();
    }
}

/// RAII wrapper over a `*mut changelog_rec` returned by `llapi_changelog_recv`.
///
/// Drop calls `llapi_changelog_free` so records are released even on error
/// paths. The underlying pointer is exposed via [`RecvBuf::as_ptr`] for use
/// with [`crate::rec::RecView`].
pub struct RecvBuf {
    rec: *mut sys::changelog_rec,
}

impl RecvBuf {
    /// Raw pointer — pass to `RecView::new` to read fields.
    #[inline]
    pub fn as_ptr(&self) -> *const sys::changelog_rec {
        self.rec
    }
}

// SAFETY: the record allocation is owned by `RecvBuf` and not shared.
unsafe impl Send for RecvBuf {}

impl Drop for RecvBuf {
    fn drop(&mut self) {
        if self.rec.is_null() {
            return;
        }
        // SAFETY: `rec` came from `llapi_changelog_recv` and hasn't been freed
        // yet. `llapi_changelog_free` takes a `**changelog_rec` and may NULL
        // the pointer.
        // H-3 fix: log on failure (matching ChangelogHandle::drop pattern).
        let rc = unsafe { sys::llapi_changelog_free(&mut self.rec) };
        if rc < 0 {
            tracing::warn!(errno = -rc, "llapi_changelog_free failed in RecvBuf::drop");
        }
        self.rec = ptr::null_mut();
    }
}

impl LustreApi {
    /// Open a changelog stream on `mdt_device` starting at `start_rec`.
    ///
    /// When `follow` is `true`, the stream uses `CHANGELOG_FLAG_FOLLOW |
    /// CHANGELOG_FLAG_BLOCK`, so [`recv_changelog`](Self::recv_changelog) will
    /// block waiting for new records. When `false`, the stream drains existing
    /// records and then returns `Ok(None)` on EOF.
    ///
    /// `CHANGELOG_FLAG_JOBID` is always set — we target Lustre 2.12+ where
    /// jobid is universally available.
    #[tracing::instrument(
        name = "lustre.open_changelog",
        skip(self),
        fields(mdt = %mdt_device, start_rec, follow),
    )]
    pub fn open_changelog(&self, mdt_device: &str, start_rec: i64, follow: bool) -> Result<ChangelogHandle> {
        let c_mdt = CString::new(mdt_device)?;
        let mut priv_: *mut c_void = ptr::null_mut();

        // CHANGELOG_FLAG_EXTRA_FLAGS enables extended record format (CLF_EXTRA_FLAGS),
        // required to receive open/xattr metadata in newer record types.
        let mut flags = sys::CHANGELOG_FLAG_JOBID | sys::CHANGELOG_FLAG_EXTRA_FLAGS;
        if follow {
            flags |= sys::CHANGELOG_FLAG_FOLLOW | sys::CHANGELOG_FLAG_BLOCK;
        }

        // SAFETY: `priv_` is a local out-parameter; `c_mdt.as_ptr()` is valid
        // for the duration of the call; the FFI contract requires start_rec
        // to be `long long` (i64 on x86_64 Linux).
        let rc = unsafe { sys::llapi_changelog_start(&mut priv_, flags, c_mdt.as_ptr(), start_rec) };
        check_rc(rc, "llapi_changelog_start")?;

        debug!(mdt = %mdt_device, "changelog stream opened");
        Ok(ChangelogHandle {
            priv_,
            mdt: mdt_device.to_owned(),
        })
    }

    /// Receive one record. Returns:
    ///
    /// * `Ok(Some(buf))` — a new record is available.
    /// * `Ok(None)` — EOF (only possible in non-follow mode).
    /// * `Err(LustreApiError::Ffi { .. })` — a fatal error.
    ///
    /// `-EINTR` is transparently retried by the caller (Phase 1b's listener
    /// loop handles signal interruption).
    #[tracing::instrument(name = "lustre.recv_changelog", skip(self, handle), fields(mdt = %handle.mdt))]
    pub fn recv_changelog(&self, handle: &ChangelogHandle) -> Result<Option<RecvBuf>> {
        let mut rec: *mut sys::changelog_rec = ptr::null_mut();

        // SAFETY: `handle.priv_` is a valid open stream; `rec` is a local
        // out-parameter. llapi_changelog_recv returns 0/1/negative errno.
        let rc = unsafe { sys::llapi_changelog_recv(handle.raw(), &mut rec) };

        if rc == 1 {
            // EOF in non-follow mode.
            return Ok(None);
        }
        if rc < 0 {
            check_rc(rc, "llapi_changelog_recv")?;
            unreachable!("check_rc returns Err on rc<0");
        }
        debug_assert!(!rec.is_null(), "recv returned 0 but rec pointer is null");
        Ok(Some(RecvBuf { rec }))
    }

    /// Acknowledge records up to (and including) `end_rec` for the given reader.
    ///
    /// Must only be called after the records have been durably committed
    /// downstream — Lustre will drop them from the MDT's changelog log once
    /// cleared, and they are not recoverable.
    #[tracing::instrument(name = "lustre.clear_changelog", skip(self))]
    pub fn clear_changelog(&self, mdt_device: &str, reader_id: &str, end_rec: i64) -> Result<()> {
        let c_mdt = CString::new(mdt_device)?;
        let c_id = CString::new(reader_id)?;
        // SAFETY: both C strings are owned locally; FFI takes `const char *`
        // and does not retain the pointers.
        let rc = unsafe { sys::llapi_changelog_clear(c_mdt.as_ptr(), c_id.as_ptr(), end_rec) };
        check_rc(rc, "llapi_changelog_clear")
    }

    /// Explicitly close a changelog handle. Equivalent to dropping it but
    /// returns any error surfaced by `llapi_changelog_fini` instead of logging.
    #[tracing::instrument(name = "lustre.close_changelog", skip(self, handle), fields(mdt = %handle.mdt))]
    pub fn close_changelog(&self, mut handle: ChangelogHandle) -> Result<()> {
        if handle.priv_.is_null() {
            return Ok(());
        }
        // SAFETY: `handle.priv_` is the pointer returned by start, still owned.
        let rc = unsafe { sys::llapi_changelog_fini(&mut handle.priv_) };
        handle.priv_ = ptr::null_mut(); // prevent Drop from double-calling fini
        check_rc(rc, "llapi_changelog_fini")
    }
}
