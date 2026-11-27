//! Read Lustre's per-client inode heat without issuing file data reads.

use std::{
    ffi::{CStr, CString, c_int},
    fs::{File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{LuFid, error::check_rc, sys};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Decay {
    pub period_seconds: u32,
    pub percentage: u8,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ReadHeat {
    pub samples: u64,
    pub bytes: u64,
}

/// Holds the client mount open, so replacement of the configured path cannot
/// redirect a batch to a different superblock.
pub struct HeatMount {
    root: File,
    identity: MountIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct MountIdentity {
    pub instance: String,
    pub source: String,
    // Distinguish independent clients on different hosts, even if Lustre
    // chooses the same instance suffix on both hosts.
    pub boot_id: String,
}

impl HeatMount {
    pub fn open(path: &Path) -> crate::Result<Self> {
        let root = open_directory(path)?;
        let source =
            mount_source(root.as_raw_fd())?.ok_or_else(|| crate::LustreApiError::NotALustreMount(path.to_owned()))?;
        Self::identify(root, source, path)
    }

    /// An unmounted path or a different Lustre source cannot share this
    /// executor's client instance. An unreadable peer remains an error.
    pub fn open_peer(path: &Path, expected_source: &str) -> crate::Result<Option<Self>> {
        let root = match open_directory(path) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        match mount_source(root.as_raw_fd())? {
            Some(source) if source == expected_source => Self::identify(root, source, path).map(Some),
            _ => Ok(None),
        }
    }

    fn identify(root: File, source: String, path: &Path) -> crate::Result<Self> {
        let anchored = CString::new(format!("/proc/self/fd/{}", root.as_raw_fd()))?;
        let mut name = [0; 64];
        // SAFETY: the path is NUL-terminated and name is a writable buffer.
        let rc = unsafe { sys::llapi_getname(anchored.as_ptr(), name.as_mut_ptr(), name.len()) };
        check_rc(rc, "llapi_getname")?;
        // SAFETY: llapi_getname returns a NUL-terminated string on success.
        let instance = unsafe { CStr::from_ptr(name.as_ptr()) }.to_str()?.to_owned();
        if instance.is_empty() || instance.contains('/') {
            return Err(crate::LustreApiError::NotALustreMount(path.to_owned()));
        }
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        Ok(Self {
            root,
            identity: MountIdentity {
                instance,
                source,
                boot_id,
            },
        })
    }

    pub fn identity(&self) -> &MountIdentity {
        &self.identity
    }

    pub fn decay(&self) -> io::Result<Option<Decay>> {
        fn parameter<T: std::str::FromStr>(dir: &Path, name: &str) -> io::Result<T> {
            std::fs::read_to_string(dir.join(name))?
                .trim()
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("invalid Lustre {name}")))
        }
        let dir = PathBuf::from("/sys/fs/lustre/llite").join(&self.identity.instance);
        match parameter::<u8>(&dir, "file_heat")? {
            0 => Ok(None),
            1 => Ok(Some(Decay {
                period_seconds: parameter(&dir, "heat_period_second")?,
                percentage: parameter(&dir, "heat_decay_percentage")?,
            })),
            _ => Err(io::Error::new(io::ErrorKind::InvalidData, "invalid Lustre file_heat")),
        }
    }

    /// A disabled inode returns None even when the ioctl succeeds.
    #[cfg(lustre_heat_abi)]
    pub fn sample(&self, fid: LuFid) -> crate::Result<Option<ReadHeat>> {
        // These optional symbols are resolved on use so older liblustreapi
        // installations can still serve deployments without heat conditions.
        let Some((open, get_heat)) = heat_functions() else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "Lustre heat API unavailable").into());
        };
        use std::os::fd::FromRawFd;
        let raw = fid.into_raw();
        let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: root is a live mount descriptor, raw is a live FID.
        let fd = unsafe { open(self.root.as_raw_fd(), &raw, flags) };
        check_rc(fd, "llapi_open_by_fid_at")?;
        // SAFETY: success returns a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            return Ok(None);
        }
        use sys::bindings::{
            lu_heat, lu_heat_flag_LU_HEAT_FLAG_OFF, obd_heat_type_OBD_HEAT_READBYTE, obd_heat_type_OBD_HEAT_READSAMPLE,
        };
        let mut heat = HeatBuffer {
            header: lu_heat {
                lh_count: HEAT_COUNT as u32,
                lh_flags: 0,
                lh_heat: Default::default(),
            },
            values: [0; HEAT_COUNT],
        };
        // SAFETY: heat includes the declared number of trailing counters.
        let rc = unsafe { get_heat(file.as_raw_fd(), std::ptr::addr_of_mut!(heat).cast()) };
        check_rc(rc, "llapi_heat_get")?;
        if heat.header.lh_count <= obd_heat_type_OBD_HEAT_READBYTE
            || heat.header.lh_flags & lu_heat_flag_LU_HEAT_FLAG_OFF != 0
        {
            return Ok(None);
        }
        Ok(Some(ReadHeat {
            samples: heat.values[obd_heat_type_OBD_HEAT_READSAMPLE as usize],
            bytes: heat.values[obd_heat_type_OBD_HEAT_READBYTE as usize],
        }))
    }

    #[cfg(not(lustre_heat_abi))]
    pub fn sample(&self, _fid: LuFid) -> crate::Result<Option<ReadHeat>> {
        // Retain root ownership on builds whose headers predate lu_heat.
        let _ = &self.root;
        Err(io::Error::new(io::ErrorKind::Unsupported, "Lustre heat API unavailable").into())
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY).open(path)
}

/// Use mountinfo for the pinned descriptor so aliases and bind mounts resolve
/// to the actual Lustre source. The source also distinguishes clusters with
/// the same filesystem and target names.
fn mount_source(fd: c_int) -> io::Result<Option<String>> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))?;
    let mount_id = info
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(str::trim)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mount ID is unavailable"))?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    let fields = mounts.lines().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        (left.split_ascii_whitespace().next() == Some(mount_id)).then(|| right.split_ascii_whitespace())
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

#[cfg(lustre_heat_abi)]
const HEAT_COUNT: usize = sys::bindings::obd_heat_type_OBD_HEAT_COUNT as usize;

// Include the flexible array storage following the generated header.
#[cfg(lustre_heat_abi)]
#[repr(C)]
struct HeatBuffer {
    header: sys::bindings::lu_heat,
    values: [u64; HEAT_COUNT],
}

#[cfg(lustre_heat_abi)]
const _: () =
    assert!(std::mem::offset_of!(HeatBuffer, values) == std::mem::offset_of!(sys::bindings::lu_heat, lh_heat));

#[cfg(lustre_heat_abi)]
type FidOpen = unsafe extern "C" fn(c_int, *const sys::lu_fid, c_int) -> c_int;
#[cfg(lustre_heat_abi)]
type HeatGet = unsafe extern "C" fn(c_int, *mut sys::bindings::lu_heat) -> c_int;

#[cfg(lustre_heat_abi)]
fn heat_functions() -> Option<(FidOpen, HeatGet)> {
    static FUNCTIONS: std::sync::OnceLock<Option<(FidOpen, HeatGet)>> = std::sync::OnceLock::new();
    *FUNCTIONS.get_or_init(|| {
        // SAFETY: these are the documented Lustre C functions from the
        // process-linked liblustreapi. Their ABI matches the two types above.
        unsafe {
            let open = libc::dlsym(libc::RTLD_DEFAULT, c"llapi_open_by_fid_at".as_ptr());
            let get = libc::dlsym(libc::RTLD_DEFAULT, c"llapi_heat_get".as_ptr());
            if open.is_null() || get.is_null() {
                return None;
            }
            Some((
                std::mem::transmute::<*mut libc::c_void, FidOpen>(open),
                std::mem::transmute::<*mut libc::c_void, HeatGet>(get),
            ))
        }
    })
}
