//! Read Lustre file layouts and the OSTs actually assigned to them.

use std::ffi::{CStr, CString, c_char, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{LustreApiError, Result, check_rc};
use crate::sys::{self, bindings};
use crate::{LuFid, LustreApi};

// Constants from lustreapi.h with ULL suffixes that bindgen leaves out.
const LLAPI_LAYOUT_DEFAULT: u64 = 0x1000_0000_0000_0002;
const LLAPI_LAYOUT_MDT: u64 = 2;
const LLAPI_LAYOUT_FOREIGN: u64 = 8;
const LUSTRE_SUPER_MAGIC: libc::c_long = 0x0bd0_0bd0;

/// Stripe settings and concrete OST indices for one layout component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripeInfo {
    /// Number of OST stripes the file is striped across.
    pub count: u32,
    /// Bytes per stripe on one OST.
    pub size: u64,
    /// Pool name, if the stripes were allocated in an OST pool.
    pub pool: Option<String>,
    /// OST index for every concrete stripe in layout order.
    pub ost_indices: Vec<u32>,
}

/// Concrete OST placement in catalog order.
#[derive(Debug, Default)]
pub struct OstLayout {
    /// Epoch microseconds when this read began. Absent for a default layout
    /// value that represents an unavailable observation.
    pub observed_at: Option<i64>,
    pub stripe_count: Option<u16>,
    pub stripe_size: Option<u32>,
    pub pool_name: Option<String>,
    pub stripe_items: Vec<u32>,
}

impl LustreApi {
    /// Read the first component's striping information for `path`.
    #[tracing::instrument(name = "lustre.get_stripe_info", skip(self), fields(path = %path.display()))]
    pub fn get_stripe_info(&self, path: &Path) -> Result<StripeInfo> {
        Layout::open(path)?
            .read_current()?
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENODATA).into())
    }

    /// Read the allocated OST layout of a regular file.
    #[tracing::instrument(skip(self), fields(path = %path.display()))]
    pub fn get_ost_layout(&self, path: &Path) -> Result<OstLayout> {
        let observed_at = layout_read_time();
        Layout::open(path)?.ost_layout(observed_at)
    }

    /// Read the live OST layout for a regular file by its FID.
    ///
    /// A deleted FID yields `None`. A live file with no allocated OSTs has
    /// an empty layout.
    #[tracing::instrument(skip(self), fields(mount = %mount.display(), %fid))]
    pub fn get_ost_layout_by_fid(&self, mount: &Path, fid: &LuFid) -> Result<Option<OstLayout>> {
        let Some(fd) = open_by_fid(mount, fid, libc::O_RDONLY | libc::O_NONBLOCK)? else {
            return Ok(None);
        };
        let observed_at = layout_read_time();
        Ok(Some(Layout::from_fd(fd.as_raw_fd())?.ost_layout(observed_at)?))
    }
}

pub(crate) fn open_by_fid(mount: &Path, fid: &LuFid, flags: libc::c_int) -> Result<Option<OwnedFd>> {
    let raw_fid = fid.into_raw();
    let flags = flags | libc::O_CLOEXEC;
    let (fd, operation) = if let Some(open_at) = fid_open_at() {
        let directory = crate::mount::open_directory(mount)?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the directory is live and stat has room for the output.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful fstatfs initialized stat.
        if unsafe { stat.assume_init() }.f_type != LUSTRE_SUPER_MAGIC {
            return Err(LustreApiError::NotALustreMount(mount.to_owned()));
        }
        // The path-based Lustre helper caches a root fd that blocks unmount.
        // SAFETY: directory and raw_fid remain live for the call.
        let fd = unsafe { open_at(directory.as_raw_fd(), &raw_fid, flags) };
        (fd, "llapi_open_by_fid_at")
    } else {
        validate_mount(mount)?;
        let path = CString::new(mount.as_os_str().as_bytes())?;
        // SAFETY: path and raw_fid remain live for the call.
        let fd = unsafe { sys::llapi_open_by_fid(path.as_ptr(), &raw_fid, flags) };
        (fd, "llapi_open_by_fid")
    };
    if fd == -libc::ENOENT || fd == -libc::ESTALE {
        return Ok(None);
    }
    check_rc(fd, operation)?;
    // SAFETY: the successful open returned an owned file descriptor.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

type FidOpenAt = unsafe extern "C" fn(libc::c_int, *const sys::lu_fid, libc::c_int) -> libc::c_int;

pub(crate) fn fid_open_at() -> Option<FidOpenAt> {
    static OPEN_AT: OnceLock<Option<FidOpenAt>> = OnceLock::new();
    *OPEN_AT.get_or_init(|| {
        // SAFETY: liblustreapi remains loaded and this is the published ABI.
        unsafe {
            let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"llapi_open_by_fid_at".as_ptr());
            (!symbol.is_null()).then(|| std::mem::transmute::<*mut c_void, FidOpenAt>(symbol))
        }
    })
}

/// Check the kernel filesystem type before a FID operation.
pub(crate) fn validate_mount(mount: &Path) -> Result<()> {
    let path = CString::new(mount.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: the path is NUL-terminated and stat has room for the output.
    if unsafe { libc::statfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: a successful statfs initialized the struct.
    if unsafe { stat.assume_init() }.f_type != LUSTRE_SUPER_MAGIC {
        return Err(LustreApiError::NotALustreMount(mount.to_owned()));
    }
    Ok(())
}

struct Layout(NonNull<c_void>);

impl Drop for Layout {
    fn drop(&mut self) {
        // SAFETY: this layout is uniquely owned and came from liblustreapi.
        unsafe { sys::llapi_layout_free(self.0.as_ptr()) };
    }
}

impl Layout {
    fn open(path: &Path) -> Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: path is NUL-terminated and valid for the call.
        let raw = unsafe { sys::llapi_layout_get_by_path(path.as_ptr(), 0) };
        Self::from_raw(raw, "llapi_layout_get_by_path")
    }

    fn from_fd(fd: RawFd) -> Result<Self> {
        // SAFETY: fd is borrowed from a live descriptor at each call site.
        let raw = unsafe { sys::llapi_layout_get_by_fd(fd, 0) };
        Self::from_raw(raw, "llapi_layout_get_by_fd")
    }

    fn from_raw(layout: *mut c_void, op: &'static str) -> Result<Self> {
        NonNull::new(layout).map(Self).ok_or_else(|| layout_error(op))
    }

    fn ost_layout(&self, observed_at: i64) -> Result<OstLayout> {
        let components = self.ost_components()?;
        let mut summary = OstLayout {
            observed_at: Some(observed_at),
            ..Default::default()
        };
        if let [component] = components.as_slice() {
            summary.stripe_count =
                Some(u16::try_from(component.count).map_err(|_| LustreApiError::IntegerOverflow("stripe count"))?);
            summary.stripe_size =
                Some(u32::try_from(component.size).map_err(|_| LustreApiError::IntegerOverflow("stripe size"))?);
        }
        if let Some(pool) = components.first().and_then(|first| first.pool.as_ref())
            && components.iter().all(|component| component.pool.as_ref() == Some(pool))
        {
            summary.pool_name = Some(pool.clone());
        }
        summary.stripe_items = components
            .into_iter()
            .flat_map(|component| component.ost_indices)
            .collect();
        Ok(summary)
    }

    fn ost_components(&self) -> Result<Vec<StripeInfo>> {
        let layout = self.0.as_ptr();
        // SAFETY: this layout remains live and is accessed only by this thread.
        if !unsafe { sys::llapi_layout_is_composite(layout) } {
            return Ok(self.read_current()?.into_iter().collect());
        }
        // SAFETY: all layout calls below use the same live layout with valid
        // positions and local output buffers.
        let rc =
            unsafe { sys::llapi_layout_comp_use(layout, bindings::llapi_layout_comp_use_LLAPI_LAYOUT_COMP_USE_FIRST) };
        check_layout_rc(rc, "llapi_layout_comp_use")?;
        let mut components = Vec::new();
        loop {
            let mut flags = 0;
            let rc = unsafe { sys::llapi_layout_comp_flags_get(layout, &mut flags) };
            check_layout_rc(rc, "llapi_layout_comp_flags_get")?;
            if flags & bindings::lov_comp_md_entry_flags_LCME_FL_INIT != 0
                && let Some(info) = self.read_current()?
            {
                components.push(info);
            }
            let rc = unsafe {
                sys::llapi_layout_comp_use(layout, bindings::llapi_layout_comp_use_LLAPI_LAYOUT_COMP_USE_NEXT)
            };
            check_layout_rc(rc, "llapi_layout_comp_use")?;
            if rc == 1 {
                return Ok(components);
            }
        }
    }

    fn read_current(&self) -> Result<Option<StripeInfo>> {
        let layout = self.0.as_ptr();
        let mut pattern = 0;
        let mut count = 0;
        let mut size = 0;
        let mut pool_buf = [0 as c_char; 128];
        // SAFETY: the layout is live and all output parameters are sized local
        // buffers. OST indices are read only up to the component stripe count.
        let rc = unsafe { sys::llapi_layout_pattern_get(layout, &mut pattern) };
        check_layout_rc(rc, "llapi_layout_pattern_get")?;
        if matches!(pattern, LLAPI_LAYOUT_MDT | LLAPI_LAYOUT_FOREIGN) {
            return Ok(None);
        }
        let rc = unsafe { sys::llapi_layout_stripe_count_get(layout, &mut count) };
        check_layout_rc(rc, "llapi_layout_stripe_count_get")?;
        if matches!(count, 0 | LLAPI_LAYOUT_DEFAULT) {
            return Ok(None);
        }
        let count = u32::try_from(count).map_err(|_| LustreApiError::IntegerOverflow("stripe count"))?;
        let rc = unsafe { sys::llapi_layout_stripe_size_get(layout, &mut size) };
        check_layout_rc(rc, "llapi_layout_stripe_size_get")?;
        let rc = unsafe { sys::llapi_layout_pool_name_get(layout, pool_buf.as_mut_ptr(), pool_buf.len()) };
        check_layout_rc(rc, "llapi_layout_pool_name_get")?;
        // SAFETY: the successful call writes a NUL-terminated pool name.
        let name = unsafe { CStr::from_ptr(pool_buf.as_ptr()) }.to_string_lossy();
        let pool = (!name.is_empty()).then(|| name.into_owned());

        let mut ost_indices = Vec::new();
        for stripe_number in 0..u64::from(count) {
            let mut index = 0;
            let rc = unsafe { sys::llapi_layout_ost_index_get(layout, stripe_number, &mut index) };
            check_layout_rc(rc, "llapi_layout_ost_index_get")?;
            if index != LLAPI_LAYOUT_DEFAULT {
                let index = u32::try_from(index).map_err(|_| LustreApiError::IntegerOverflow("OST index"))?;
                ost_indices.push(index);
            }
        }
        Ok((!ost_indices.is_empty()).then_some(StripeInfo {
            count,
            size,
            pool,
            ost_indices,
        }))
    }
}

/// Only an initialized single-component RAID0 layout can be moved.
pub(crate) fn simple_layout(fd: RawFd) -> Result<Option<StripeInfo>> {
    let layout = match Layout::from_fd(fd) {
        Err(LustreApiError::Ffi {
            errno: libc::ENODATA, ..
        }) => return Ok(None),
        result => result?,
    };
    let ptr = layout.0.as_ptr();
    let mut pattern = 0;
    // SAFETY: the layout is live and pattern is a writable u64.
    if unsafe { sys::llapi_layout_is_composite(ptr) } {
        return Ok(None);
    }
    check_layout_rc(
        unsafe { sys::llapi_layout_pattern_get(ptr, &mut pattern) },
        "llapi_layout_pattern_get",
    )?;
    if pattern != 0 {
        // LLAPI_LAYOUT_RAID0
        return Ok(None);
    }
    Ok(layout
        .read_current()?
        .filter(|stripe| stripe.ost_indices.len() == stripe.count as usize))
}

impl StripeInfo {
    pub(crate) fn observation(self, observed_at: i64) -> Result<OstLayout> {
        Ok(OstLayout {
            observed_at: Some(observed_at),
            stripe_count: Some(u16::try_from(self.count).map_err(|_| LustreApiError::IntegerOverflow("stripe count"))?),
            stripe_size: Some(u32::try_from(self.size).map_err(|_| LustreApiError::IntegerOverflow("stripe size"))?),
            pool_name: self.pool,
            stripe_items: self.ost_indices,
        })
    }
}

pub(crate) fn layout_read_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

// The layout accessors return -1 and set errno. llapi_open_by_fid instead
// returns the negative errno directly.
fn check_layout_rc(rc: i32, op: &'static str) -> Result<()> {
    if rc < 0 { Err(layout_error(op)) } else { Ok(()) }
}

fn layout_error(op: &'static str) -> LustreApiError {
    let error = std::io::Error::last_os_error();
    LustreApiError::Ffi {
        op,
        errno: error.raw_os_error().unwrap_or(0),
        strerror: error.to_string(),
    }
}
