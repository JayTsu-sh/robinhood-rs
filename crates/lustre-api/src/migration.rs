//! Blocking, FID-preserving migration between Lustre OST pools.

use std::{
    collections::BTreeSet,
    ffi::{CStr, CString},
    fs::{File, Metadata},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
        },
    },
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{
    HsmState, LuFid, LustreApi,
    error::{LustreApiError, check_rc},
    stripe::{OstLayout, StripeInfo, layout_read_time, open_by_fid, simple_layout},
};

// Public lustre_user.h flag. Bindgen omits the macro expression.
const SWAP_LAYOUTS_CHECK_DV1: u64 = 1;

/// Stripe settings fixed at admission and persisted with the operation.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPlan {
    pub source_pool: String,
    pub target_pool: String,
    pub stripe_count: u16,
    pub stripe_size: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileVersion {
    pub size: u64,
    pub mtime: i64,
    pub mtime_nanos: i64,
    pub ctime: i64,
    pub ctime_nanos: i64,
}

impl From<&Metadata> for FileVersion {
    fn from(metadata: &Metadata) -> Self {
        Self {
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nanos: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nanos: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("{0}")]
    Deferred(&'static str),
    #[error("migration failed before layout exchange: {0}")]
    BeforeSwap(#[from] LustreApiError),
    #[error("layout exchange needs reconciliation: {0}")]
    Uncertain(String),
}

impl From<io::Error> for MigrationError {
    fn from(error: io::Error) -> Self {
        LustreApiError::from(error).into()
    }
}

pub enum MigrationPlacement {
    Target(Box<VerifiedPlacement>),
    SourceUnchanged,
    SourceChanged,
    Missing,
    Unresolved,
}

/// Placement and inode attributes read after reopening the original FID.
pub struct VerifiedPlacement {
    pub layout: OstLayout,
    pub metadata: Metadata,
}

/// Local ownership persists inside the blocking task even if its async waiter
/// is cancelled. The policy store also maintains ownership across restarts.
static ACTIVE: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

pub struct MigrationOwnership(String);

impl MigrationOwnership {
    pub fn acquire(filesystem: &str) -> Result<Self, MigrationError> {
        let mut active = ACTIVE
            .lock()
            .map_err(|_| MigrationError::Deferred("migration_lock_unavailable"))?;
        if !active.insert(filesystem.to_owned()) {
            return Err(MigrationError::Deferred("filesystem_migration_busy"));
        }
        Ok(Self(filesystem.to_owned()))
    }
}

impl Drop for MigrationOwnership {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE.lock() {
            active.remove(&self.0);
        }
    }
}

struct GroupLock<'a> {
    source: &'a File,
    id: libc::c_int,
}

impl<'a> GroupLock<'a> {
    fn acquire(file: &'a File) -> Result<Self, MigrationError> {
        let mut random = [0; 4];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let id = (i32::from_ne_bytes(random) & i32::MAX).max(1);
        let fd = file.as_raw_fd();
        // SAFETY: fd refers to the live source file and id is nonzero.
        check_rc(unsafe { crate::sys::llapi_group_lock(fd, id) }, "llapi_group_lock")?;
        Ok(Self { source: file, id })
    }
}

impl Drop for GroupLock<'_> {
    fn drop(&mut self) {
        // SAFETY: each call site drops the guard before closing the source.
        let rc = unsafe { crate::sys::llapi_group_unlock(self.source.as_raw_fd(), self.id) };
        if rc < 0 {
            tracing::warn!(rc, "Lustre group unlock failed; closing source descriptor");
        }
    }
}

pub fn valid_pool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        && !matches!(name, "ignore" | "inherit" | "none")
}

impl LustreApi {
    /// Enumerate concrete OST members of an unqualified pool on this mount.
    pub fn pool_members(&self, mount: &Path, pool: &str) -> crate::error::Result<BTreeSet<u32>> {
        if !valid_pool_name(pool) {
            return Err(LustreApiError::RecordParse("invalid Lustre pool name"));
        }
        let fs = self.hsm_filesystem(mount)?;
        self.qualified_pool_members(&fs, pool)
    }

    fn qualified_pool_members(&self, filesystem: &str, pool: &str) -> crate::error::Result<BTreeSet<u32>> {
        let qualified = CString::new(format!("{filesystem}.{pool}"))?;
        let mut capacity = 64;
        loop {
            let mut list = vec![std::ptr::null_mut(); capacity];
            let mut buffer = vec![0 as libc::c_char; capacity * 64];
            // SAFETY: both buffers are writable and the C API fills list with
            // pointers into buffer. Sizes are bounded by the loop.
            let rc = unsafe {
                crate::sys::llapi_get_poolmembers(
                    qualified.as_ptr(),
                    list.as_mut_ptr(),
                    capacity as i32,
                    buffer.as_mut_ptr(),
                    buffer.len() as i32,
                )
            };
            if rc == -libc::EOVERFLOW && capacity < 65536 {
                capacity *= 2;
                continue;
            }
            check_rc(rc, "llapi_get_poolmembers")?;
            let prefix = format!("{filesystem}-OST");
            return list[..rc as usize]
                .iter()
                .map(|&member| {
                    // SAFETY: a returned member is a NUL-terminated string in buffer.
                    let name = unsafe { CStr::from_ptr(member) }.to_str()?;
                    let index = name
                        .strip_prefix(&prefix)
                        .map(|value| value.strip_suffix("_UUID").unwrap_or(value))
                        .and_then(|value| u32::from_str_radix(value, 16).ok());
                    index.ok_or(LustreApiError::RecordParse("unexpected Lustre OST pool member"))
                })
                .collect();
        }
    }

    /// Read a supported source layout without moving the file.
    pub fn migration_source_layout(&self, mount: &Path, fid: &LuFid) -> Result<StripeInfo, MigrationError> {
        let fd = open_by_fid(mount, fid, libc::O_RDONLY | libc::O_NONBLOCK)?
            .ok_or(MigrationError::Deferred("fid_missing"))?;
        simple_layout(fd.as_raw_fd())?.ok_or(MigrationError::Deferred("unsupported_or_unallocated_layout"))
    }

    /// Move data under a Lustre group lock, then verify placement from a fresh FID open.
    #[tracing::instrument(skip(self, plan, expected), fields(mount = %mount.display(), %fid))]
    pub fn migrate_pool(
        &self, mount: &Path, fid: &LuFid, plan: &MigrationPlan, expected: FileVersion, deadline: Instant,
    ) -> Result<VerifiedPlacement, MigrationError> {
        let fs = self.hsm_filesystem(mount)?;
        let ownership = MigrationOwnership::acquire(&fs)?;
        self.migrate_reserved_pool(mount, fid, plan, expected, deadline, ownership)
    }

    /// Carry process-local ownership into the blocking task before it is scheduled.
    #[tracing::instrument(skip(self, plan, expected, ownership), fields(mount = %mount.display(), %fid))]
    pub fn migrate_reserved_pool(
        &self, mount: &Path, fid: &LuFid, plan: &MigrationPlan, expected: FileVersion, deadline: Instant,
        ownership: MigrationOwnership,
    ) -> Result<VerifiedPlacement, MigrationError> {
        if !valid_pool_name(&plan.source_pool)
            || !valid_pool_name(&plan.target_pool)
            || plan.source_pool == plan.target_pool
            || plan.stripe_count == 0
            || plan.stripe_size == 0
            || !plan.stripe_size.is_multiple_of(65536)
        {
            return Err(MigrationError::Deferred("invalid_pool_migration_plan"));
        }
        if self.hsm_filesystem(mount)? != ownership.0 {
            return Err(MigrationError::Deferred("filesystem_identity_changed"));
        }
        check_deadline(deadline)?;
        eligible_hsm(self, mount, fid)?;
        let src = open_source(mount, fid)?.ok_or(MigrationError::Deferred("fid_missing"))?;
        let group = GroupLock::acquire(&src)?;
        let source = src.metadata()?;
        if !source.is_file() || source.nlink() == 0 || FileVersion::from(&source) != expected {
            return Err(MigrationError::Deferred("source_generation_changed"));
        }
        check_inode_flags(&src)?;
        eligible_hsm(self, mount, fid)?;
        let current =
            simple_layout(src.as_raw_fd())?.ok_or(MigrationError::Deferred("unsupported_or_unallocated_layout"))?;
        let source_members = self.pool_members(mount, &plan.source_pool)?;
        if !is_in_pool(&current, &plan.source_pool, &source_members) {
            return Err(MigrationError::Deferred("source_pool_changed"));
        }
        let target_members = self.pool_members(mount, &plan.target_pool)?;
        if target_members.len() < usize::from(plan.stripe_count) {
            return Err(MigrationError::Deferred("target_pool_has_insufficient_osts"));
        }
        let dst = volatile_file(mount, &src, &source, plan)?;
        let mut version = 0;
        // SAFETY: the source is open and version is a valid output. flags=0
        // avoids taking an OST extent lock while the group lock is held.
        check_rc(
            unsafe { crate::sys::llapi_get_data_version(src.as_raw_fd(), &mut version, 0) },
            "llapi_get_data_version",
        )?;
        copy_sparse(&src, &dst, expected.size, deadline)?;
        set_times(&dst, &source)?;
        dst.sync_all()?;
        let destination =
            simple_layout(dst.as_raw_fd())?.ok_or(MigrationError::Deferred("target_layout_not_allocated"))?;
        if !matches_target(
            &destination,
            plan,
            &self.qualified_pool_members(&ownership.0, &plan.target_pool)?,
        ) {
            return Err(MigrationError::Deferred("volatile_layout_mismatch"));
        }
        if FileVersion::from(&src.metadata()?) != expected {
            return Err(MigrationError::Deferred("source_generation_changed"));
        }
        eligible_hsm(self, mount, fid)?;
        check_deadline(deadline)?;

        // SAFETY: both fds are open for writing. gid=0 uses the existing
        // source group lock. DV1 detects writes before committing the swap.
        let rc = unsafe {
            crate::sys::llapi_fswap_layouts_grouplock(
                src.as_raw_fd(),
                dst.as_raw_fd(),
                version,
                0,
                0,
                SWAP_LAYOUTS_CHECK_DV1,
            )
        };
        check_rc(rc, "llapi_fswap_layouts_grouplock").map_err(|error| MigrationError::Uncertain(error.to_string()))?;
        drop(group);
        drop(dst);
        drop(src);
        self.verify_destination(mount, fid, plan, &destination)
    }

    fn verify_destination(
        &self, mount: &Path, fid: &LuFid, plan: &MigrationPlan, expected: &StripeInfo,
    ) -> Result<VerifiedPlacement, MigrationError> {
        let verify = || {
            let fd = open_by_fid(mount, fid, libc::O_RDONLY | libc::O_NONBLOCK)?
                .ok_or(MigrationError::Deferred("fid_missing"))?;
            let fd = File::from(fd);
            let observed = layout_read_time();
            let current =
                simple_layout(fd.as_raw_fd())?.ok_or(MigrationError::Deferred("unsupported_destination_layout"))?;
            let members = self.pool_members(mount, &plan.target_pool)?;
            if &current != expected || !matches_target(&current, plan, &members) {
                return Err(MigrationError::Deferred("reopened_fid_layout_mismatch"));
            }
            Ok(VerifiedPlacement {
                layout: current.observation(observed)?,
                metadata: fd.metadata()?,
            })
        };
        verify().map_err(|error| MigrationError::Uncertain(error.to_string()))
    }

    /// Check the live layout after obtaining exclusive migration ownership.
    /// Inspect placement under a group lock. Application writes after a
    /// successful swap can change the generation while keeping the target layout.
    pub fn reconcile_pool_migration(
        &self, mount: &Path, fid: &LuFid, plan: &MigrationPlan, expected: FileVersion, filesystem_name: &str,
    ) -> Result<MigrationPlacement, MigrationError> {
        if self.hsm_filesystem(mount)? != filesystem_name {
            return Err(MigrationError::Deferred("filesystem_identity_changed"));
        }
        let _ownership = MigrationOwnership::acquire(filesystem_name)?;
        let Some(src) = open_source(mount, fid)? else {
            return Ok(MigrationPlacement::Missing);
        };
        let _group = GroupLock::acquire(&src)?;
        let observed = layout_read_time();
        let Some(layout) = simple_layout(src.as_raw_fd())? else {
            return Ok(MigrationPlacement::Unresolved);
        };
        let metadata = src.metadata()?;
        let members = self.pool_members(mount, &plan.target_pool)?;
        if matches_target(&layout, plan, &members) {
            return Ok(MigrationPlacement::Target(Box::new(VerifiedPlacement {
                layout: layout.observation(observed)?,
                metadata,
            })));
        }
        if is_in_pool(
            &layout,
            &plan.source_pool,
            &self.pool_members(mount, &plan.source_pool)?,
        ) {
            return Ok(if FileVersion::from(&metadata) == expected {
                MigrationPlacement::SourceUnchanged
            } else {
                MigrationPlacement::SourceChanged
            });
        }
        Ok(MigrationPlacement::Unresolved)
    }
}

fn is_in_pool(layout: &StripeInfo, pool: &str, members: &BTreeSet<u32>) -> bool {
    layout.pool.as_deref() == Some(pool)
        && !layout.ost_indices.is_empty()
        && layout.ost_indices.iter().all(|ost| members.contains(ost))
}

fn matches_target(layout: &StripeInfo, plan: &MigrationPlan, members: &BTreeSet<u32>) -> bool {
    is_in_pool(layout, &plan.target_pool, members)
        && layout.count == u32::from(plan.stripe_count)
        && layout.size == u64::from(plan.stripe_size)
}

fn open_source(mount: &Path, fid: &LuFid) -> crate::error::Result<Option<File>> {
    Ok(open_by_fid(mount, fid, libc::O_RDWR | libc::O_NONBLOCK | libc::O_NOATIME)?.map(File::from))
}

fn eligible_hsm(api: &LustreApi, mount: &Path, fid: &LuFid) -> Result<(), MigrationError> {
    let path: PathBuf = mount.join(".lustre/fid").join(fid.to_string());
    let state = api.hsm_state_and_action(&path).map_err(|error| {
        tracing::debug!(%fid, %error, "migration could not verify HSM state");
        MigrationError::Deferred("hsm_state_unavailable")
    })?;
    check_migration_hsm(&state).map_err(MigrationError::Deferred)
}

pub fn check_migration_hsm(state: &crate::HsmUserStateInfo) -> Result<(), &'static str> {
    if state.has_active_action() {
        return Err("hsm_action_active");
    }
    let known_online_hints = HsmState::NOARCHIVE | HsmState::NORELEASE;
    if !(state.states - known_online_hints).is_empty() || state.archive_id != 0 {
        return Err("hsm_state_ineligible");
    }
    Ok(())
}

fn check_inode_flags(file: &File) -> Result<(), MigrationError> {
    let mut flags: libc::c_long = 0;
    // SAFETY: file is live and flags can hold the Linux ioctl output.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    const FS_ENCRYPT_FL: libc::c_long = 0x800;
    if flags & FS_ENCRYPT_FL != 0 {
        return Err(MigrationError::Deferred("encrypted_file"));
    }
    Ok(())
}

fn volatile_file(mount: &Path, src: &File, meta: &Metadata, plan: &MigrationPlan) -> Result<File, MigrationError> {
    let mut mdt = -1;
    // SAFETY: src is open and mdt is a writable c_int.
    check_rc(
        unsafe { crate::sys::llapi_file_fget_mdtidx(src.as_raw_fd(), &mut mdt) },
        "llapi_file_fget_mdtidx",
    )?;
    let path = CString::new(mount.as_os_str().as_bytes()).map_err(LustreApiError::from)?;
    let pool = CString::new(plan.target_pool.as_bytes()).map_err(LustreApiError::from)?;
    // SAFETY: the C struct accepts zero defaults for its scalar fields and the
    // flexible OST list is unused when lsp_is_specific is false.
    let mut stripe: crate::sys::bindings::llapi_stripe_param = unsafe { std::mem::zeroed() };
    stripe.lsp_pool = pool.as_ptr().cast_mut();
    stripe.lsp_stripe_offset = -1;
    stripe.lsp_stripe_size = plan.stripe_size.into();
    stripe.lsp_stripe_count = plan.stripe_count.into();
    // SAFETY: strings and stripe are valid for the call and the target mode is valid.
    let fd = unsafe {
        crate::sys::llapi_create_volatile_param(path.as_ptr(), mdt, libc::O_RDWR | libc::O_CLOEXEC, 0o600, &stripe)
    };
    check_rc(fd, "llapi_create_volatile_param")?;
    // SAFETY: the successful call gave us a new descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    // SAFETY: file remains open and these are the source UID and GID.
    if unsafe { libc::fchown(fd, meta.uid(), meta.gid()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(file)
}

fn set_times(file: &File, metadata: &Metadata) -> io::Result<()> {
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    // SAFETY: file is live and times points to both required entries.
    if unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn copy_sparse(src: &File, dst: &File, size: u64, deadline: Instant) -> Result<(), MigrationError> {
    let mut buffer = vec![0; 1024 * 1024];
    let mut cursor = 0;
    while cursor < size {
        check_deadline(deadline)?;
        let Some(mut data) = seek(src, cursor, libc::SEEK_DATA)? else {
            break;
        };
        let hole = seek(src, data, libc::SEEK_HOLE)?.unwrap_or(size).min(size);
        if data < cursor || hole <= data {
            return Err(MigrationError::Deferred("invalid_sparse_extent"));
        }
        while data < hole {
            check_deadline(deadline)?;
            let len = (hole - data).min(buffer.len() as u64) as usize;
            src.read_exact_at(&mut buffer[..len], data)?;
            dst.write_all_at(&buffer[..len], data)?;
            data += len as u64;
        }
        cursor = hole;
    }
    dst.set_len(size)?;
    Ok(())
}

fn seek(file: &File, offset: u64, whence: i32) -> io::Result<Option<u64>> {
    let offset = i64::try_from(offset).map_err(io::Error::other)?;
    // SAFETY: file is open. whence is a SEEK_DATA or SEEK_HOLE constant.
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result >= 0 {
        return Ok(Some(result as u64));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENXIO) {
        Ok(None)
    } else {
        Err(error)
    }
}

fn check_deadline(deadline: Instant) -> Result<(), MigrationError> {
    if Instant::now() >= deadline {
        Err(MigrationError::Deferred("migration_deadline_elapsed"))
    } else {
        Ok(())
    }
}
