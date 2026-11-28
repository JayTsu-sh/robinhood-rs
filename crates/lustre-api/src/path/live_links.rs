//! Complete, mount-relative link evidence for migration admission.

use std::{
    collections::BTreeSet,
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    time::Instant,
};

use crate::{
    LuFid, LustreApi,
    migration::{MigrationError, check_deadline},
    mount::{mount_id, open_directory},
    stripe::open_by_fid,
    sys,
};

use super::RBH_PATH_MAX;

const INCOMPLETE: MigrationError = MigrationError::Deferred("incomplete_link_evidence");
const CHANGED: MigrationError = MigrationError::Deferred("link_evidence_changed");

#[derive(Debug, PartialEq, Eq)]
pub struct LiveLinks {
    mount_id: String,
    root_fid: LuFid,
    root: PathBuf,
    links: BTreeSet<PathBuf>,
}

impl LiveLinks {
    /// A name outside the configured root is represented by `None`.
    pub fn relative_paths(&self) -> impl ExactSizeIterator<Item = Option<&Path>> {
        self.links.iter().map(|path| path.strip_prefix(&self.root).ok())
    }
}

impl LustreApi {
    /// Probe the configured daemon's access to the live Lustre namespace.
    pub fn verify_migration_namespace(&self, mount: &Path) -> crate::Result<()> {
        let directory = open_directory(mount)?;
        let root_fid = descriptor_fid(&directory)?;
        fid_link_path(&directory, &root_fid, 0)?;
        Ok(())
    }

    pub fn migration_link_paths(
        &self, mount: &Path, fid: &LuFid, deadline: Instant,
    ) -> Result<LiveLinks, MigrationError> {
        check_deadline(deadline)?;
        let source = open_by_fid(mount, fid, libc::O_PATH)
            .map_err(|_| INCOMPLETE)?
            .map(File::from)
            .ok_or(INCOMPLETE)?;
        self.migration_link_paths_for_file(mount, fid, &source, deadline)
    }

    /// Recheck using the open source descriptor held by the migration.
    pub fn migration_link_paths_for_file(
        &self, mount: &Path, fid: &LuFid, source: &File, deadline: Instant,
    ) -> Result<LiveLinks, MigrationError> {
        let source_metadata = source.metadata().map_err(|_| INCOMPLETE)?;
        let count = u32::try_from(source_metadata.nlink()).map_err(|_| INCOMPLETE)?;
        if !source_metadata.is_file() || count == 0 || regular_file_fid(source)? != *fid {
            return Err(INCOMPLETE);
        }
        let device = mount_id(source.as_raw_fd()).map_err(|_| INCOMPLETE)?;
        let first = observe_links(mount, fid, &device, count, deadline)?;
        let second = observe_links(mount, fid, &device, count, deadline)?;
        let after = source.metadata().map_err(|_| INCOMPLETE)?;
        if first != second
            || (
                source_metadata.nlink(),
                source_metadata.ctime(),
                source_metadata.ctime_nsec(),
            ) != (after.nlink(), after.ctime(), after.ctime_nsec())
        {
            return Err(CHANGED);
        }
        Ok(second)
    }
}

fn observe_links(
    mount: &Path, fid: &LuFid, device: &str, count: u32, deadline: Instant,
) -> Result<LiveLinks, MigrationError> {
    check_deadline(deadline)?;
    let directory = open_directory(mount).map_err(|_| INCOMPLETE)?;
    let mount_id = mount_id(directory.as_raw_fd()).map_err(|_| INCOMPLETE)?;
    if mount_id != device {
        return Err(CHANGED);
    }
    let root_fid = descriptor_fid(&directory).map_err(|_| INCOMPLETE)?;
    let root = fid_link_path(&directory, &root_fid, 0).map_err(link_lookup_error)?;
    let mut links = BTreeSet::new();
    // Request each link number explicitly. The kernel can fall back to the
    // first name for an out-of-range index, so duplicate paths fail closed.
    for index in 0..count {
        check_deadline(deadline)?;
        let path = fid_link_path(&directory, fid, index).map_err(link_lookup_error)?;
        if let Ok(relative) = path.strip_prefix(&root)
            && !resolves_to(&directory, relative, fid, device, deadline)?
        {
            return Err(CHANGED);
        }
        if path.as_os_str().is_empty() || !links.insert(path) {
            return Err(INCOMPLETE);
        }
    }
    Ok(LiveLinks {
        mount_id,
        root_fid,
        root,
        links,
    })
}

// Lustre getinfo_fid2path with its trailing path buffer allocated inline.
#[repr(C, packed)]
struct FidPathRequest {
    fid: sys::lu_fid,
    record: u64,
    link: u32,
    path_len: u32,
    path: [u8; RBH_PATH_MAX],
}

fn fid_link_path(directory: &File, fid: &LuFid, index: u32) -> crate::Result<PathBuf> {
    let mut request = FidPathRequest {
        fid: fid.into_raw(),
        record: 0,
        link: index,
        path_len: RBH_PATH_MAX as u32,
        path: [0; RBH_PATH_MAX],
    };
    // SAFETY: directory is live and request holds both the ABI header and
    // path_len writable bytes. This ioctl also works before liblustreapi
    // exposed the descriptor-based llapi_fid2path_at wrapper.
    let rc = unsafe {
        libc::ioctl(
            directory.as_raw_fd(),
            sys::bindings::RBH_OBD_IOC_FID2PATH.into(),
            &mut request,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let len = request
        .path
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(crate::LustreApiError::RecordParse("unterminated Lustre link path"))?;
    let mut path = PathBuf::new();
    // DNE paths can include repeated separators between MDT segments.
    for component in request.path[..len]
        .split(|&byte| byte == b'/')
        .filter(|part| !part.is_empty())
    {
        if matches!(component, b"." | b"..") {
            return Err(crate::LustreApiError::RecordParse("invalid Lustre link path"));
        }
        path.push(OsStr::from_bytes(component));
    }
    Ok(path)
}

fn link_lookup_error(error: crate::LustreApiError) -> MigrationError {
    match error {
        crate::LustreApiError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            MigrationError::Deferred("link_namespace_permission_denied")
        }
        _ => INCOMPLETE,
    }
}

fn descriptor_fid(file: &File) -> crate::Result<LuFid> {
    let mut fid = LuFid::ZERO.into_raw();
    // SAFETY: file is live and fid is a writable output of the required type.
    let rc = unsafe { sys::llapi_fd2fid(file.as_raw_fd(), &mut fid) };
    crate::error::check_rc(rc, "llapi_fd2fid")?;
    Ok(LuFid::from_raw(fid))
}

fn regular_file_fid(file: &File) -> Result<LuFid, MigrationError> {
    if !file.metadata().map_err(|_| INCOMPLETE)?.is_file() {
        return Err(CHANGED);
    }
    let fid = match descriptor_fid(file) {
        // Older liblustreapi uses an ioctl that requires a readable fd.
        Err(crate::LustreApiError::Ffi { errno: libc::EBADF, .. }) => {
            let readable = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOATIME)
                .open(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .map_err(|_| INCOMPLETE)?;
            descriptor_fid(&readable)
        }
        result => result,
    };
    fid.map_err(|_| INCOMPLETE)
}

fn resolves_to(
    directory: &File, relative: &Path, fid: &LuFid, device: &str, deadline: Instant,
) -> Result<bool, MigrationError> {
    let mut parts = relative.as_os_str().as_bytes().split(|&byte| byte == b'/').peekable();
    let mut directory = directory.try_clone().map_err(|_| INCOMPLETE)?;
    while let Some(part) = parts.next() {
        check_deadline(deadline)?;
        let name = CString::new(part).map_err(|_| INCOMPLETE)?;
        let mut flags = libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        if parts.peek().is_some() {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: directory is live and name is NUL-terminated. Components
        // supplied by fid_link_path cannot traverse a parent or contain '/'.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(INCOMPLETE);
        }
        // SAFETY: the successful open returned a new owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    check_deadline(deadline)?;
    let resolved_mount = mount_id(directory.as_raw_fd()).map_err(|_| INCOMPLETE)?;
    Ok(resolved_mount == device && regular_file_fid(&directory)? == *fid)
}
