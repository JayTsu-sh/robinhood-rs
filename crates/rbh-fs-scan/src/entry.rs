//! Build an [`EntryRow`] from a filesystem path by statting and querying Lustre APIs.

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use lustre_api::LustreApi;
use lustre_api::fid::LuFid;
use rbh_entry_store::model::{EntryKind, EntryRow};

use crate::PosixEntry;
use crate::ScanError;

/// Add Lustre-native identity and layout metadata to a POSIX walk result.
#[tracing::instrument(skip(lustre, entry), fields(path = %entry.path.display()))]
pub fn enrich_lustre(lustre: &LustreApi, entry: &PosixEntry) -> Result<EntryRow, ScanError> {
    let fid = lustre.path_to_fid(&entry.path)?;
    let parent_fid = entry
        .parent_path
        .as_ref()
        .map(|parent| lustre.path_to_fid(parent))
        .transpose()?;
    let (stripe_count, stripe_size, pool_name) = if entry.kind == EntryKind::File {
        lustre
            .get_stripe_info(&entry.path)
            .map(|layout| (Some(layout.count as u16), Some(layout.size as u32), layout.pool))
            .unwrap_or((None, None, None))
    } else {
        (None, None, None)
    };
    Ok(EntryRow {
        fid,
        parent_fid,
        name: entry.name.clone(),
        kind: entry.kind,
        size: entry.size,
        blocks: entry.blocks,
        uid: entry.uid,
        gid: entry.gid,
        projid: 0,
        mode: entry.mode,
        nlink: entry.nlink,
        atime: entry.atime,
        mtime: entry.mtime,
        ctime: entry.ctime,
        stripe_count,
        stripe_size,
        pool_name,
        sm_status: serde_json::json!({}),
        last_seen: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        depth: entry.depth,
    })
}

/// Build an [`EntryRow`] from filesystem metadata.
///
/// 1. `symlink_metadata` — POSIX stat without following symlinks
/// 2. `llapi_path2fid` — resolve FID
/// 3. For regular files: `get_stripe_info` — stripe count, size, pool
/// 4. Set `last_seen` to current time
///
/// `parent_fid` is passed in from the walk (parent directory's FID).
#[tracing::instrument(skip(lustre, parent_fid), fields(path = %path.display()))]
pub fn build_entry(
    lustre: &LustreApi, path: &Path, parent_fid: Option<LuFid>, depth: u32,
) -> Result<EntryRow, ScanError> {
    // 1. Stat
    let meta = std::fs::symlink_metadata(path).map_err(|e| ScanError::Io {
        path: path.display().to_string(),
        source: e,
    })?;

    let kind = metadata_to_kind(&meta);
    let name = path
        .file_name()
        .map(|n| Bytes::copy_from_slice(n.as_encoded_bytes()))
        .unwrap_or_else(|| Bytes::from_static(b""));

    // 2. FID
    let fid = lustre.path_to_fid(path)?;

    // 3. Stripe info (files only)
    let (stripe_count, stripe_size, pool_name) = if kind == EntryKind::File {
        match lustre.get_stripe_info(path) {
            Ok(info) => (Some(info.count as u16), Some(info.size as u32), info.pool),
            Err(_) => (None, None, None),
        }
    } else {
        (None, None, None)
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    Ok(EntryRow {
        fid,
        parent_fid,
        name,
        kind,
        size: meta.size(),
        blocks: meta.blocks(),
        uid: meta.uid(),
        gid: meta.gid(),
        projid: 0, // Lustre project ID requires ioctl; leave 0 for now
        mode: meta.mode(),
        nlink: meta.nlink() as u32,
        atime: meta.atime(),
        mtime: meta.mtime(),
        ctime: meta.ctime(),
        stripe_count,
        stripe_size,
        pool_name,
        sm_status: serde_json::json!({}),
        last_seen: now,
        depth,
    })
}

fn metadata_to_kind(meta: &std::fs::Metadata) -> EntryKind {
    use std::os::unix::fs::FileTypeExt;
    let ft = meta.file_type();
    if ft.is_file() {
        EntryKind::File
    } else if ft.is_dir() {
        EntryKind::Directory
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_char_device() {
        EntryKind::CharDevice
    } else if ft.is_block_device() {
        EntryKind::BlockDevice
    } else if ft.is_fifo() {
        EntryKind::Fifo
    } else {
        EntryKind::Socket // fallback for any remaining type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_kind_variants_distinct() {
        assert_ne!(EntryKind::File, EntryKind::Directory);
        assert_ne!(EntryKind::Directory, EntryKind::Symlink);
        assert_ne!(EntryKind::CharDevice, EntryKind::BlockDevice);
    }
}
