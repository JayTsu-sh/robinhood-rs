//! FID ↔ path conversions.
//!
//! Thin wrappers over `llapi_fid2path` and `llapi_path2fid`. Both functions
//! operate in terms of a Lustre device name (e.g. `"testfs-MDT0000"`) or an
//! existing filesystem path; internally they issue an ioctl to the MDT.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use core::ffi::{c_char, c_int, c_longlong};

use crate::LustreApi;
use crate::error::{Result, check_rc};
use crate::fid::LuFid;
use crate::sys;

/// Maximum path length returned by `llapi_fid2path`. Matches Linux's PATH_MAX.
const RBH_PATH_MAX: usize = 4096;

impl LustreApi {
    /// Resolve a FID to a path.
    ///
    /// The `device` argument is what liblustreapi calls a "device name", but in
    /// practice on a Lustre client you pass a path on the target mount (e.g.
    /// `/lustre`) or the filesystem name (e.g. `testfs`). MDT kernel device
    /// names like `testfs-MDT0000` only work on the MDS host where the device
    /// actually exists.
    ///
    /// When `device` is a mount point, the returned path is **absolute** (e.g.
    /// `/lustre/foo/bar`). When `device` is a bare fsname, the returned path
    /// is **relative** to the mount (e.g. `foo/bar`). Callers should pass the
    /// form they want the result in.
    ///
    /// Hardlinks: `llapi_fid2path` iterates all names via the `linkno` parameter
    /// when called repeatedly; this wrapper returns only the first link
    /// (linkno = 0). Phase 2's `rbh-entry-store::names` table tracks the full
    /// hardlink set from changelog updates.
    #[tracing::instrument(name = "lustre.fid_to_path", skip(self), fields(fid = %fid))]
    pub fn fid_to_path(&self, device: &str, fid: &LuFid) -> Result<PathBuf> {
        let c_dev = CString::new(device)?;
        let fidstr = fid.to_string();
        let c_fid = CString::new(fidstr)?;
        let mut buf: [c_char; RBH_PATH_MAX] = [0; RBH_PATH_MAX];
        let mut recno: c_longlong = 0;
        let mut linkno: c_int = 0;

        // SAFETY: all pointers are valid for the duration of the call; buf is
        // sized to RBH_PATH_MAX and llapi_fid2path writes a NUL-terminated
        // string (the Lustre contract).
        let rc = unsafe {
            sys::llapi_fid2path(
                c_dev.as_ptr(),
                c_fid.as_ptr(),
                buf.as_mut_ptr(),
                RBH_PATH_MAX as c_int,
                &mut recno,
                &mut linkno,
            )
        };
        check_rc(rc, "llapi_fid2path")?;

        // Find NUL terminator.
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        // SAFETY: `buf` is `[c_char; N]`, which is `[i8; N]`. Reinterpret as bytes.
        let bytes: &[u8] = unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, len) };
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes).to_os_string()))
    }

    /// Resolve an existing path to its FID.
    ///
    /// The path must exist on a Lustre mount; otherwise `llapi_path2fid`
    /// returns `-ENOTTY` (not a Lustre file).
    #[tracing::instrument(name = "lustre.path_to_fid", skip(self), fields(path = %path.display()))]
    pub fn path_to_fid(&self, path: &Path) -> Result<LuFid> {
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        let mut raw = sys::lu_fid {
            f_seq: 0,
            f_oid: 0,
            f_ver: 0,
        };
        // SAFETY: `raw` is a local out-parameter; `c_path.as_ptr()` lives for
        // the call duration.
        let rc = unsafe { sys::llapi_path2fid(c_path.as_ptr(), &mut raw) };
        check_rc(rc, "llapi_path2fid")?;
        Ok(LuFid::from_raw(raw))
    }
}
