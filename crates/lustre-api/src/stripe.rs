//! Lustre stripe / layout info.
//!
//! Reads the LOV EA of a file via `llapi_layout_get_by_path` and derives the
//! basic parameters robinhood-rs policies filter on: stripe count, stripe
//! size (bytes per object), and the optional pool name.
//!
//! OST index enumeration (`llapi_layout_ost_index_get`) and the stripe pattern
//! (`llapi_layout_pattern_get` → raid0 / mdt / mirrored) are not surfaced in
//! Phase 1a — add them here when a consumer needs them.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use core::ffi::{c_char, c_void};

use serde::{Deserialize, Serialize};

use crate::LustreApi;
use crate::error::{LustreApiError, Result, check_rc};
use crate::sys;

/// A file's stripe layout. `None` in [`LustreApi::get_stripe_info`] for entries
/// that have no layout (directories, symlinks).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripeInfo {
    /// Number of OST stripes the file is striped across.
    pub count: u32,
    /// Bytes per stripe on one OST.
    pub size: u64,
    /// Pool name, if the file was created in an OST pool. Empty → no pool.
    pub pool: Option<String>,
    /// OST index for every concrete stripe in layout order.
    pub ost_indices: Vec<u32>,
}

impl LustreApi {
    /// Read the LOV EA for `path` and return its basic stripe parameters.
    #[tracing::instrument(name = "lustre.get_stripe_info", skip(self), fields(path = %path.display()))]
    pub fn get_stripe_info(&self, path: &Path) -> Result<StripeInfo> {
        let c_path = CString::new(path.as_os_str().as_bytes())?;

        // SAFETY: c_path outlives the call; flags=0 requests the default view.
        let layout = unsafe { sys::llapi_layout_get_by_path(c_path.as_ptr(), 0) };
        if layout.is_null() {
            return Err(LustreApiError::Ffi {
                op: "llapi_layout_get_by_path",
                errno: errno_from_libc(),
                strerror: std::io::Error::last_os_error().to_string(),
            });
        }

        // RAII: ensure llapi_layout_free runs even on early return.
        struct LayoutGuard(*mut c_void);
        impl Drop for LayoutGuard {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: pointer came from llapi_layout_get_by_path and
                    // has not been freed yet.
                    unsafe { sys::llapi_layout_free(self.0) };
                }
            }
        }
        let _guard = LayoutGuard(layout);

        let mut count: u64 = 0;
        // SAFETY: `count` is a local out-parameter; layout is a valid pointer.
        let rc = unsafe { sys::llapi_layout_stripe_count_get(layout, &mut count) };
        check_rc(rc, "llapi_layout_stripe_count_get")?;

        let mut size: u64 = 0;
        // SAFETY: same.
        let rc = unsafe { sys::llapi_layout_stripe_size_get(layout, &mut size) };
        check_rc(rc, "llapi_layout_stripe_size_get")?;

        // Pool name — we pass a fixed buffer. LOV_MAXPOOLNAME is 15 historically,
        // 63 in more recent Lustre; 128 is defensive.
        let mut pool_buf: [c_char; 128] = [0; 128];
        // SAFETY: pool_buf is sized for any Lustre pool name.
        let rc = unsafe { sys::llapi_layout_pool_name_get(layout, pool_buf.as_mut_ptr(), pool_buf.len()) };
        // Treat "no pool" (empty string, rc == 0) the same as "rc < 0"-with-ENODATA:
        // both just mean the file is not in a pool.
        let pool = if rc == 0 {
            // SAFETY: pool_buf is NUL-terminated on success.
            let cstr = unsafe { CStr::from_ptr(pool_buf.as_ptr()) };
            let s = cstr.to_string_lossy().into_owned();
            if s.is_empty() { None } else { Some(s) }
        } else {
            None
        };

        let mut ost_indices = Vec::with_capacity(count as usize);
        for stripe_number in 0..count {
            let mut index = 0_u64;
            // SAFETY: layout remains valid under `_guard`; index is a local
            // out-parameter and stripe_number is bounded by the reported count.
            let rc = unsafe { sys::llapi_layout_ost_index_get(layout, stripe_number, &mut index) };
            check_rc(rc, "llapi_layout_ost_index_get")?;
            ost_indices.push(u32::try_from(index).unwrap_or(u32::MAX));
        }

        Ok(StripeInfo {
            count: u32::try_from(count).unwrap_or(u32::MAX),
            size,
            pool,
            ost_indices,
        })
    }
}

/// Best-effort errno retrieval for the `llapi_layout_get_by_path` NULL-return path.
///
/// liblustreapi sets `errno` on failure; we read via libc::__errno_location.
fn errno_from_libc() -> i32 {
    // SAFETY: `__errno_location` returns a pointer to the thread-local errno.
    unsafe { *libc::__errno_location() }
}
