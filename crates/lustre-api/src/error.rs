//! Error types and `check_rc` helpers for mapping liblustreapi return codes.
//!
//! liblustreapi functions return either `0` on success, `1` on changelog EOF, or
//! a negative errno (e.g. `-EINVAL`, `-ENOENT`) on failure. `check_rc` and
//! `check_rc_hsm` turn the negative cases into typed `LustreApiError` variants,
//! including a human-readable message from `libc::strerror`.

use std::ffi::CStr;
use std::path::PathBuf;

use core::ffi::c_int;

/// All fallible operations in `lustre-api` return this error type.
#[derive(Debug, thiserror::Error)]
pub enum LustreApiError {
    /// A low-level liblustreapi call returned `-errno`. `op` identifies the
    /// function that failed; operators can search for it in logs.
    #[error("liblustreapi `{op}` failed: errno {errno} ({strerror})")]
    Ffi {
        op: &'static str,
        errno: i32,
        strerror: String,
    },

    /// An HSM-specific call failed. The `op` distinguishes get-state / set-state / request
    /// so the circuit-breaker middleware can decide whether to retry.
    #[error("HSM operation `{op:?}` failed: errno {errno} ({strerror})")]
    Hsm { op: HsmOp, errno: i32, strerror: String },

    #[error("path `{0}` is not a Lustre mount or is inaccessible")]
    NotALustreMount(PathBuf),

    #[error("MDT `{0}` is not active")]
    InactiveMdt(String),

    #[error("changelog record parse error: {0}")]
    RecordParse(&'static str),

    #[error("string contains interior NUL byte")]
    PathContainsNul(#[from] std::ffi::NulError),

    #[error("string is not valid UTF-8")]
    InvalidUtf8(#[from] std::str::Utf8Error),

    #[error("io error")]
    Io(#[from] std::io::Error),

    #[error("integer out of range converting {0}")]
    IntegerOverflow(&'static str),
}

/// Kind of HSM operation — used in `LustreApiError::Hsm` for telemetry attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HsmOp {
    StateGet,
    StateSet,
    Request,
}

/// Short alias used throughout the crate.
pub type Result<T> = std::result::Result<T, LustreApiError>;

/// Convert a liblustreapi return code to `Result<()>`.
///
/// Returns `Ok(())` for `rc >= 0`, or `Err(LustreApiError::Ffi { op, ... })`
/// with the decoded errno string for `rc < 0`.
pub(crate) fn check_rc(rc: c_int, op: &'static str) -> Result<()> {
    if rc < 0 {
        let errno = -rc;
        Err(LustreApiError::Ffi {
            op,
            errno,
            strerror: strerror_for(errno),
        })
    } else {
        Ok(())
    }
}

/// HSM-specific variant of [`check_rc`]. Produces `LustreApiError::Hsm` instead of
/// `Ffi` so telemetry can distinguish HSM failures from changelog/MDT failures.
pub(crate) fn check_rc_hsm(rc: c_int, op: HsmOp) -> Result<()> {
    if rc < 0 {
        let errno = -rc;
        Err(LustreApiError::Hsm {
            op,
            errno,
            strerror: strerror_for(errno),
        })
    } else {
        Ok(())
    }
}

/// Best-effort `strerror(3)` lookup. Falls back to `"unknown error"`.
fn strerror_for(errno: i32) -> String {
    // SAFETY: `libc::strerror` is guaranteed to return a pointer to a static
    // string describing the errno, or NULL for invalid errno values.
    unsafe {
        let ptr = libc::strerror(errno);
        if ptr.is_null() {
            String::from("unknown error")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}
