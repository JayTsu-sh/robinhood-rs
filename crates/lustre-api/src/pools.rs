//! Read OST pool inventory from the specific Lustre client backing a mount.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io,
    mem::MaybeUninit,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use crate::{
    LuFid, LustreApi, LustreApiError, Result,
    mdt::{SpaceUsage, read_ost_usage},
    mount::{LustreMount, client_instance, lustre_mount, open_directory},
    sys,
};

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
        PoolMount::open(mount)?.pool_members(pool)
    }
}

/// A discovered Lustre pool. Membership can fail independently of pool discovery.
#[derive(Debug)]
pub struct OstPool {
    pub name: String,
    pub members: Result<BTreeSet<u32>>,
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MountIdentity {
    Unique(u64),
    Client {
        mount_id: String,
        instance: String,
        source: String,
    },
}

/// Keeps the verified mount open while resolving the client and its pool data.
pub struct PoolMount {
    descriptor: File,
    path: PathBuf,
    mount: LustreMount,
}

impl PoolMount {
    pub fn open(path: &Path) -> Result<Self> {
        let not_mounted = || LustreApiError::NotALustreMount(path.to_owned());
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(path)
            .map_err(|error| match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => not_mounted(),
                _ => error.into(),
            })?;
        Self::from_descriptor(directory, path)
    }

    pub(crate) fn from_file(file: &File, mount: &Path) -> Result<Self> {
        Self::from_descriptor(file.try_clone()?, mount)
    }

    fn from_descriptor(descriptor: File, path: &Path) -> Result<Self> {
        let mount =
            lustre_mount(descriptor.as_raw_fd())?.ok_or_else(|| LustreApiError::NotALustreMount(path.to_owned()))?;
        Ok(Self {
            descriptor,
            path: path.to_owned(),
            mount,
        })
    }

    /// Resolve the directory FID on the same pinned mount as pool membership.
    pub fn directory_fid(&self) -> Result<LuFid> {
        // Older liblustreapi versions issue an ioctl on this descriptor.
        let directory = self.open_pinned_directory()?;
        let mut fid = LuFid::ZERO.into_raw();
        // SAFETY: the pinned directory is live and fid is writable for the call.
        let rc = unsafe { sys::llapi_fd2fid(directory.as_raw_fd(), &mut fid) };
        crate::error::check_rc(rc, "llapi_fd2fid")?;
        Ok(LuFid::from_raw(fid))
    }

    pub fn read_only(&self) -> bool {
        self.mount.read_only
    }

    pub fn source(&self) -> &str {
        &self.mount.source
    }

    pub fn active_mdt_names(&self) -> Result<Vec<String>> {
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", self.descriptor.as_raw_fd()));
        crate::LustreApi.active_mdt_names(&anchored)
    }

    /// Prefer the kernel's non-reusable mount ID. On older kernels also verify
    /// the live Lustre client and source before using any previous observations.
    pub fn identity(&self) -> Option<MountIdentity> {
        self.unique_mount_id().map(MountIdentity::Unique).or_else(|| {
            let instance = client_instance(&self.descriptor, &self.path).ok()?;
            Some(MountIdentity::Client {
                mount_id: self.mount.id.clone(),
                instance,
                source: self.mount.source.clone(),
            })
        })
    }

    fn unique_mount_id(&self) -> Option<u64> {
        const STATX_MNT_ID_UNIQUE: u32 = 0x4000; // Linux 6.8, linux/stat.h
        let mut stat = MaybeUninit::<libc::statx>::uninit();
        // SAFETY: the pinned descriptor is live and stat points to writable storage.
        let rc = unsafe {
            libc::statx(
                self.descriptor.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_STATX_DONT_SYNC,
                STATX_MNT_ID_UNIQUE,
                stat.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return None;
        }
        // SAFETY: statx initialized the buffer on success. Older kernels omit the requested field.
        let stat = unsafe { stat.assume_init() };
        (stat.stx_mask & STATX_MNT_ID_UNIQUE != 0).then_some(stat.stx_mnt_id)
    }

    /// List known OST indices, including inactive targets and targets outside pools.
    pub fn ost_targets(&self) -> Result<OstTargets> {
        let (lov, prefix) = self.client_lov()?;
        match read_lov_parameter(&lov, "target_obd", |path| fs::read_to_string(path)) {
            Ok(targets) => targets
                .lines()
                .map(|line| {
                    line.split_ascii_whitespace()
                        .next()
                        .and_then(|index| index.strip_suffix(':'))
                        .and_then(|index| index.parse().ok())
                        .ok_or(LustreApiError::RecordParse("unexpected Lustre OST target index"))
                })
                .collect::<Result<BTreeSet<_>>>()
                .map(|known| OstTargets {
                    known,
                    unverified: BTreeSet::new(),
                }),
            Err(error) if matches!(error.kind(), io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound) => {
                connected_ost_indices(&lov, &prefix)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Open the pinned mount for per-OST capacity measurement.
    pub fn ost_space_reader(&self) -> Result<OstSpaceReader> {
        Ok(OstSpaceReader {
            directory: self.open_pinned_directory()?,
        })
    }

    fn open_pinned_directory(&self) -> io::Result<File> {
        open_directory(Path::new(&format!("/proc/self/fd/{}", self.descriptor.as_raw_fd())))
    }

    fn pool_location(&self) -> Result<(PathBuf, String)> {
        let (lov, prefix) = self.client_lov()?;
        let pools = read_lov_parameter(&lov, "pools", |path| fs::read_dir(path).map(|_| path.to_owned()))?;
        Ok((pools, prefix))
    }

    fn client_lov(&self) -> Result<(String, String)> {
        let client = client_instance(&self.descriptor, &self.path)?;
        let (filesystem, instance) = client
            .rsplit_once('-')
            .ok_or(LustreApiError::RecordParse("unexpected Lustre client instance"))?;
        Ok((format!("{filesystem}-clilov-{instance}"), format!("{filesystem}-OST")))
    }

    pub fn ost_pools(&self) -> Result<Vec<OstPool>> {
        let (pools, prefix) = self.pool_location()?;
        fs::read_dir(pools)?
            .map(|entry| {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| LustreApiError::RecordParse("unexpected Lustre pool name"))?;
                let members = read_members(&entry.path(), &prefix);
                Ok(OstPool { name, members })
            })
            .collect()
    }

    pub fn pool_members(&self, pool: &str) -> Result<BTreeSet<u32>> {
        if !valid_pool_name(pool) {
            return Err(LustreApiError::RecordParse("invalid Lustre pool name"));
        }
        let (pools, prefix) = self.pool_location()?;
        read_members(&pools.join(pool), &prefix)
    }
}

/// OSTs verified by the client and candidate indices that need confirmation.
pub struct OstTargets {
    pub known: BTreeSet<u32>,
    pub unverified: BTreeSet<u32>,
}

/// A missing slot and an OST without available counters are distinct.
pub enum OstCapacity {
    Absent,
    Unavailable,
    Measured(SpaceUsage),
}

/// Read-only handle shared by concurrent OST queries on the same mount.
pub struct OstSpaceReader {
    directory: File,
}

impl OstSpaceReader {
    pub fn usage(&self, index: u32) -> Result<OstCapacity> {
        match read_ost_usage(
            self.directory.as_raw_fd(),
            sys::LL_STATFS_LOV | sys::LL_STATFS_NODELAY,
            index,
        ) {
            Err(LustreApiError::Ffi {
                errno: libc::ENODATA, ..
            }) => Ok(OstCapacity::Unavailable),
            Ok(Some(usage)) => Ok(OstCapacity::Measured(SpaceUsage::from(&usage))),
            Ok(None) => Ok(OstCapacity::Absent),
            Err(error) => Err(error),
        }
    }
}

fn connected_ost_indices(lov: &str, prefix: &str) -> Result<OstTargets> {
    let params = Path::new("/sys/fs/lustre/lov").join(lov);
    let count: u32 = fs::read_to_string(params.join("numobd"))?
        .trim()
        .parse()
        .map_err(|_| LustreApiError::RecordParse("unexpected Lustre OST count"))?;
    let mut indices = BTreeSet::new();
    for entry in fs::read_dir(params.join("target_obds"))? {
        let name = entry?.file_name();
        let index = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .and_then(|name| name.split_once("-osc-"))
            .and_then(|(index, _)| u32::from_str_radix(index, 16).ok())
            .ok_or(LustreApiError::RecordParse("unexpected Lustre OSC name"))?;
        indices.insert(index);
    }
    // numobd describes the slot range. The ioctl can distinguish a hole
    // from a known target without counters when a slot has no OSC link.
    let unverified = (0..count).filter(|index| !indices.contains(index)).collect();
    Ok(OstTargets {
        known: indices,
        unverified,
    })
}

fn read_lov_parameter<T>(lov: &str, parameter: &str, read: impl Fn(&Path) -> io::Result<T>) -> io::Result<T> {
    // These are Lustre's parameter roots. The name-based pool APIs can choose
    // another mount's LOV when two mounted clusters share a filesystem name.
    let mut error = io::Error::new(io::ErrorKind::NotFound, format!("Lustre client {parameter} not found"));
    for root in [
        "/sys/fs/lustre",
        "/sys/kernel/debug/lustre",
        "/proc/fs/lustre",
        "/proc/sys/lustre",
    ] {
        let path = PathBuf::from(root).join("lov").join(lov).join(parameter);
        match read(&path) {
            Ok(value) => return Ok(value),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {}
            Err(cause) => error = cause,
        }
    }
    Err(error)
}

fn read_members(path: &Path, prefix: &str) -> Result<BTreeSet<u32>> {
    fs::read_to_string(path)?
        .lines()
        .map(|name| {
            name.strip_prefix(prefix)
                .map(|value| value.strip_suffix("_UUID").unwrap_or(value))
                .and_then(|value| u32::from_str_radix(value, 16).ok())
                .ok_or(LustreApiError::RecordParse("unexpected Lustre OST pool member"))
        })
        .collect()
}
