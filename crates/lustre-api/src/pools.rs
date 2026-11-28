//! Read OST pool inventory from the specific Lustre client backing a mount.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io,
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

use crate::{
    LustreApi, LustreApiError, Result,
    mount::{client_instance, lustre_mount},
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

/// Keeps the verified mount open while resolving the client and its pool data.
pub struct PoolMount<'a> {
    descriptor: File,
    path: &'a Path,
    read_only: bool,
}

impl<'a> PoolMount<'a> {
    pub fn open(path: &'a Path) -> Result<Self> {
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

    pub(crate) fn from_file(file: &File, mount: &'a Path) -> Result<Self> {
        Self::from_descriptor(file.try_clone()?, mount)
    }

    fn from_descriptor(descriptor: File, path: &'a Path) -> Result<Self> {
        let mount =
            lustre_mount(descriptor.as_raw_fd())?.ok_or_else(|| LustreApiError::NotALustreMount(path.to_owned()))?;
        Ok(Self {
            descriptor,
            path,
            read_only: mount.read_only,
        })
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    fn pool_location(&self) -> Result<(PathBuf, String)> {
        let client = client_instance(&self.descriptor, self.path)?;
        let (filesystem, instance) = client
            .rsplit_once('-')
            .ok_or(LustreApiError::RecordParse("unexpected Lustre client instance"))?;
        let lov = format!("{filesystem}-clilov-{instance}");
        Ok((pool_directory(&lov)?, format!("{filesystem}-OST")))
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

    pub(crate) fn pool_members(&self, pool: &str) -> Result<BTreeSet<u32>> {
        if !valid_pool_name(pool) {
            return Err(LustreApiError::RecordParse("invalid Lustre pool name"));
        }
        let (pools, prefix) = self.pool_location()?;
        read_members(&pools.join(pool), &prefix)
    }
}

fn pool_directory(lov: &str) -> io::Result<PathBuf> {
    // These are Lustre's parameter roots. The name-based pool APIs can choose
    // another mount's LOV when two mounted clusters share a filesystem name.
    let mut error = io::Error::new(io::ErrorKind::NotFound, "Lustre client pool directory not found");
    for root in [
        "/sys/fs/lustre",
        "/sys/kernel/debug/lustre",
        "/proc/fs/lustre",
        "/proc/sys/lustre",
    ] {
        let path = PathBuf::from(root).join("lov").join(lov).join("pools");
        match fs::read_dir(&path) {
            Ok(_) => return Ok(path),
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
