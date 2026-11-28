//! Pin a mount and identify its filesystem source.

use std::{
    ffi::{CStr, CString, c_int},
    fs::{File, OpenOptions},
    io,
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY).open(path)
}

/// Use mountinfo for the pinned descriptor so aliases and bind mounts resolve
/// to the actual Lustre source. The source also distinguishes clusters with
/// the same filesystem and target names.
pub(crate) fn mount_source(fd: c_int) -> io::Result<Option<String>> {
    Ok(lustre_mount(fd)?.map(|mount| mount.source))
}

pub(crate) struct LustreMount {
    pub id: String,
    pub source: String,
    pub read_only: bool,
}

pub(crate) fn lustre_mount(fd: c_int) -> io::Result<Option<LustreMount>> {
    let mount_id = mount_id(fd)?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    let fields = mounts.lines().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        let mut mount = left.split_ascii_whitespace();
        (mount.next() == Some(mount_id.as_str())).then(|| (mount.nth(4), right.split_ascii_whitespace()))
    });
    let Some((mount_flags, mut fields)) = fields else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "mount source is unavailable"));
    };
    if fields.next() != Some("lustre") {
        return Ok(None);
    }
    let source = fields
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Lustre mount source is empty"))?;
    let readonly = |flags: &str| flags.split(',').any(|flag| flag == "ro");
    let read_only = match (mount_flags, fields.next()) {
        (Some(mount), Some(superblock)) => readonly(mount) || readonly(superblock),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Lustre mount flags are missing",
            ));
        }
    };
    Ok(Some(LustreMount {
        id: mount_id,
        source: source.to_owned(),
        read_only,
    }))
}

pub(crate) fn mount_id(fd: c_int) -> io::Result<String> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(|id| id.trim().to_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mount ID is unavailable"))
}

pub(crate) fn client_instance(file: &File, path: &Path) -> crate::Result<String> {
    let anchored = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    let mut name = [0; 64];
    // SAFETY: the path is NUL-terminated and name is a writable buffer.
    let rc = unsafe { crate::sys::llapi_getname(anchored.as_ptr(), name.as_mut_ptr(), name.len()) };
    crate::error::check_rc(rc, "llapi_getname")?;
    let bytes = name.map(|byte| byte as u8);
    let instance = CStr::from_bytes_until_nul(&bytes)
        .map_err(|_| crate::LustreApiError::RecordParse("invalid Lustre client instance"))?
        .to_str()?;
    if instance.is_empty() || instance.contains('/') {
        return Err(crate::LustreApiError::NotALustreMount(path.to_owned()));
    }
    Ok(instance.to_owned())
}
