//! Domain types for the entry catalog.

use bytes::Bytes;
use lustre_api::LuFid;
use serde::{Deserialize, Serialize};

/// Entry kind — matches the `kind` TINYINT column in `entries`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum EntryKind {
    File = 0,
    Directory = 1,
    Symlink = 2,
    CharDevice = 3,
    BlockDevice = 4,
    Fifo = 5,
    Socket = 6,
}

impl EntryKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::File),
            1 => Some(Self::Directory),
            2 => Some(Self::Symlink),
            3 => Some(Self::CharDevice),
            4 => Some(Self::BlockDevice),
            5 => Some(Self::Fifo),
            6 => Some(Self::Socket),
            _ => None,
        }
    }
}

/// One row in `rbh_entries.entries`. FID-keyed, Lustre/Linux-only.
/// See `entry_row_replaces_nasentry.md` for the design rationale.
#[derive(Debug, Clone)]
pub struct EntryRow {
    pub fid: LuFid,
    pub parent_fid: Option<LuFid>,
    pub name: Bytes,
    pub kind: EntryKind,
    pub size: u64,
    pub blocks: u64,
    pub uid: u32,
    pub gid: u32,
    pub projid: u32,
    pub mode: u32,
    pub nlink: u32,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub stripe_count: Option<u16>,
    pub stripe_size: Option<u32>,
    pub pool_name: Option<String>,
    pub sm_status: serde_json::Value,
    pub last_seen: i64,
}

/// One row in `rbh_entries.removed_entries`.
#[derive(Debug, Clone)]
pub struct RemovedEntry {
    pub fid: LuFid,
    pub parent_fid: Option<LuFid>,
    pub name: Bytes,
    pub kind: EntryKind,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub sm_status: serde_json::Value,
    pub rm_time: i64,
}
