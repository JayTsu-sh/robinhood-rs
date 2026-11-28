//! Pin a mount and identify its filesystem source.

use std::{
    ffi::c_int,
    fs::{File, OpenOptions},
    io,
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
    let mount_id = mount_id(fd)?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    let fields = mounts.lines().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        (left.split_ascii_whitespace().next() == Some(mount_id.as_str())).then(|| right.split_ascii_whitespace())
    });
    let Some(mut fields) = fields else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "mount source is unavailable"));
    };
    if fields.next() != Some("lustre") {
        return Ok(None);
    }
    fields
        .next()
        .map(|source| Some(source.to_owned()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Lustre mount source is empty"))
}

pub(crate) fn mount_id(fd: c_int) -> io::Result<String> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(|id| id.trim().to_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mount ID is unavailable"))
}
