#![allow(clippy::items_after_test_module)]

//! Changelog ingest — consumes [`EventBatch`]es from the changelog listener
//! and applies them to the entry store.
//!
//! For creation events (Create, Mkdir, Mknod, Softlink), we stat the file
//! via `fid_to_path` + `symlink_metadata` to populate the full [`EntryRow`].
//! For deletion events (Unlink with last_link, Rmdir), we call `remove_entry`.
//! For Rename, we update parent_fid + name and handle rename-overwrite.
//! For metadata events (Close, SetAttr, Truncate, MTime, CTime), we re-stat
//! the file to refresh changed fields.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use lustre_api::LustreApi;
use lustre_changelog::{ChangelogEvent, EventAck, ListenerHandle};
use rbh_entry_store::model::{EntryKind, EntryRow};
use rbh_entry_store::store::EntryStore;

/// Run the changelog ingest loop. Consumes batches from the listener,
/// applies events to the entry store, and sends acks back.
///
/// Exits when the listener channel closes or the cancel token fires.
#[tracing::instrument(name = "changelog.ingest", skip_all)]
pub async fn ingest_loop(
    mut handle: ListenerHandle, entry_store: EntryStore, mount_path: PathBuf, cancel: CancellationToken,
) {
    tracing::info!("changelog ingest loop started");

    loop {
        let batch = tokio::select! {
            batch = handle.events.recv() => {
                match batch {
                    Some(b) => b,
                    None => {
                        tracing::info!("changelog event channel closed");
                        break;
                    }
                }
            }
            _ = cancel.cancelled() => {
                tracing::info!("changelog ingest cancelled");
                break;
            }
        };

        let mdt = batch.mdt.clone();
        let max_index = batch.max_index;
        let event_count = batch.events.len();

        tracing::info!(
            mdt = %mdt,
            events = event_count,
            min_idx = batch.min_index,
            max_idx = max_index,
            "processing changelog batch"
        );

        let mut applied = 0u64;
        let mut skipped = 0u64;
        let mut errors = 0u64;

        // Count events into the per-MDT, per-type metric before any
        // apply work so even skipped / errored events show up on the
        // Grafana panel.
        {
            use std::collections::HashMap;
            let mut by_type: HashMap<&'static str, u64> = HashMap::new();
            for envelope in &batch.events {
                *by_type.entry(envelope.event.kind_name()).or_insert(0) += 1;
            }
            for (ty, n) in by_type {
                rbh_observability::metrics::CHANGELOG_EVENTS
                    .with_label_values(&[mdt.as_str(), ty])
                    .inc_by(n);
            }
        }

        for envelope in &batch.events {
            match apply_event(&entry_store, &mount_path, &envelope.event, envelope.time).await {
                Ok(true) => applied += 1,
                Ok(false) => skipped += 1,
                Err(e) => {
                    errors += 1;
                    tracing::warn!(
                        fid = %envelope.event.fid(),
                        kind = envelope.event.kind_name(),
                        error = %e,
                        "failed to apply changelog event"
                    );
                }
            }
        }

        tracing::info!(
            mdt = %mdt,
            max_idx = max_index,
            applied,
            skipped,
            errors,
            "changelog batch complete"
        );

        // Ack after durable commit.
        if handle
            .acks
            .send(EventAck {
                mdt,
                committed_index: max_index,
            })
            .await
            .is_err()
        {
            tracing::warn!("ack channel closed — listener may have stopped");
            break;
        }
    }

    tracing::info!("changelog ingest loop exiting");
}

/// Apply a single changelog event to the entry store.
/// Returns `Ok(true)` if the store was modified, `Ok(false)` if skipped.
async fn apply_event(
    store: &EntryStore, mount: &Path, event: &ChangelogEvent, event_time: i64,
) -> anyhow::Result<bool> {
    match event {
        // ── Creation events: stat the new file and upsert ──
        ChangelogEvent::Create { fid, parent, name, .. }
        | ChangelogEvent::Mkdir { fid, parent, name, .. }
        | ChangelogEvent::Mknod { fid, parent, name, .. } => {
            let kind = match event {
                ChangelogEvent::Create { .. } => EntryKind::File,
                ChangelogEvent::Mkdir { .. } => EntryKind::Directory,
                _ => EntryKind::File, // Mknod — approximate
            };
            match stat_entry_by_fid(mount, fid, Some(*parent), name.clone(), kind).await {
                Ok(entry) => {
                    store.upsert_entry(&entry).await?;
                    Ok(true)
                }
                Err(e) => {
                    // File may have been deleted between event and processing.
                    tracing::debug!(fid = %fid, error = %e, "stat failed for create event — file may be gone");
                    Ok(false)
                }
            }
        }

        ChangelogEvent::Softlink { fid, parent, name, .. } => {
            match stat_entry_by_fid(mount, fid, Some(*parent), name.clone(), EntryKind::Symlink).await {
                Ok(entry) => {
                    store.upsert_entry(&entry).await?;
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }

        ChangelogEvent::Hardlink { fid, parent, name } => {
            // Update the entry's parent/name to the new link location.
            // A full hardlink implementation would insert into the names table.
            if let Some(mut entry) = store.get_entry(fid).await? {
                entry.parent_fid = Some(*parent);
                entry.name = name.clone();
                entry.nlink = entry.nlink.saturating_add(1);
                entry.last_seen = now_secs();
                store.upsert_entry(&entry).await?;
                Ok(true)
            } else {
                // Entry not in catalog — try stat
                match stat_entry_by_fid(mount, fid, Some(*parent), name.clone(), EntryKind::File).await {
                    Ok(entry) => {
                        store.upsert_entry(&entry).await?;
                        Ok(true)
                    }
                    Err(_) => Ok(false),
                }
            }
        }

        // ── Deletion events ──
        ChangelogEvent::Unlink { fid, last_link, .. } => {
            tracing::debug!(fid = %fid, last_link = *last_link, "processing Unlink event");
            if *last_link {
                // DNE rename stitching hedge: a cross-MDT rename can
                // surface as a last-link UNLNK on the source MDT even
                // though the file still lives under a different parent
                // on another MDT. Check via llapi_fid2path before
                // moving to removed_entries — if the FID still
                // resolves, this is a rename we'll catch via the
                // companion Rename event on the other MDT.
                let live = fid_still_live(mount, fid).await;
                if live {
                    tracing::info!(
                        fid = %fid,
                        "last-link UNLNK but FID still resolves — treating as rename-away, skipping delete"
                    );
                    return Ok(false);
                }
                store.remove_entry(fid, event_time).await?;
                Ok(true)
            } else {
                // Decrement nlink if the entry exists.
                if let Some(mut entry) = store.get_entry(fid).await? {
                    entry.nlink = entry.nlink.saturating_sub(1);
                    entry.last_seen = now_secs();
                    store.upsert_entry(&entry).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }

        ChangelogEvent::Rmdir { fid, .. } => {
            store.remove_entry(fid, event_time).await?;
            Ok(true)
        }

        // ── Rename ──
        ChangelogEvent::Rename { fid, parent, name, .. } => {
            tracing::debug!(
                fid = %fid,
                dst_name = %String::from_utf8_lossy(name),
                "processing Rename event"
            );

            // Handle rename-overwrite: if an entry already occupies the
            // destination (parent, name), it was displaced and must be removed.
            //
            // We first try (parent_fid, name) lookup. If that doesn't find a
            // match, we fall back to a name-only search. The fallback handles
            // entries from initial scans or old changelog replays where
            // parent_fid may not yet be set correctly.
            let displaced = store
                .lookup_by_parent_name(parent, name)
                .await
                .ok()
                .flatten()
                .filter(|dfid| *dfid != *fid);

            if let Some(displaced_fid) = displaced {
                tracing::info!(
                    displaced_fid = %displaced_fid,
                    dst_name = %String::from_utf8_lossy(name),
                    "Rename-overwrite: removing displaced entry"
                );
                store.remove_entry(&displaced_fid, event_time).await?;
            }

            let existing = store.get_entry(fid).await?;
            if let Some(mut entry) = existing {
                entry.parent_fid = Some(*parent);
                entry.name = name.clone();
                entry.last_seen = now_secs();
                store.upsert_entry(&entry).await?;
                Ok(true)
            } else {
                match stat_entry_by_fid(mount, fid, Some(*parent), name.clone(), EntryKind::File).await {
                    Ok(entry) => {
                        store.upsert_entry(&entry).await?;
                        Ok(true)
                    }
                    Err(e) => {
                        tracing::debug!(fid = %fid, error = %e, "Rename: fallback stat failed");
                        Ok(false)
                    }
                }
            }
        }

        // ── Metadata-change events: re-stat the file ──
        ChangelogEvent::Close { fid, parent, name } => {
            // Close is the reliable "data changed" signal.
            // cr_pfid is often zero for CLOSE records — only pass parent if non-zero.
            let parent_opt = if parent.is_zero() { None } else { Some(*parent) };
            restat_entry(store, mount, fid, parent_opt, name.clone()).await
        }

        ChangelogEvent::Truncate { fid }
        | ChangelogEvent::SetAttr { fid }
        | ChangelogEvent::MTime { fid }
        | ChangelogEvent::CTime { fid } => restat_entry(store, mount, fid, None, Bytes::new()).await,

        ChangelogEvent::Hsm {
            fid,
            hsm_event,
            hsm_flags,
            hsm_error,
        } => apply_hsm_event(store, fid, *hsm_event, *hsm_flags, *hsm_error).await,

        // ── Events we skip for now ──
        ChangelogEvent::XAttr { .. } | ChangelogEvent::Layout { .. } => Ok(false),
    }
}

/// Translate a Lustre `hsm_event` + `hsm_flags` into a JSON patch and
/// merge it into `entries.sm_status`. Returns `true` when a row was
/// updated (i.e. the entry is already in the catalog).
///
/// The `hsm_event` mapping follows `enum hsm_event` in
/// `<lustre/lustre_user.h>`:
///   0 archive  1 restore  2 cancel  3 release  4 remove  5 state
///
/// `hsm_flags` carries the packed state bits from `cr_flags`; bit 0 is
/// `CLF_HSM_DIRTY`. Non-zero `hsm_error` means the coordinator reported
/// a failure — we record the code and the inferred operation but leave
/// the previous state in place.
/// Pure helper: translate Lustre HSM event fields into the sm_status
/// JSON patch. Separated from `apply_hsm_event` so it can be unit-
/// tested without a DB.
fn build_hsm_patch(hsm_event: u8, hsm_flags: u8, hsm_error: u8, now: i64) -> serde_json::Value {
    let op = match hsm_event {
        0 => "archive",
        1 => "restore",
        2 => "cancel",
        3 => "release",
        4 => "remove",
        5 => "state",
        _ => "unknown",
    };
    let mut patch = serde_json::json!({
        "hsm_last_op": op,
        "hsm_last_event_ts": now,
        "hsm_dirty": (hsm_flags & 0x01) != 0,
    });
    if hsm_error != 0 {
        patch["hsm_last_error"] = serde_json::json!(hsm_error);
    } else {
        let state = match hsm_event {
            0 => Some("archived"),
            1 => Some("archived"), // restored = back to archived + present
            3 => Some("released"),
            4 => Some("none"),
            _ => None,
        };
        if let Some(s) = state {
            patch["hsm_state"] = serde_json::json!(s);
        }
    }
    patch
}

async fn apply_hsm_event(
    store: &EntryStore, fid: &lustre_api::LuFid, hsm_event: u8, hsm_flags: u8, hsm_error: u8,
) -> anyhow::Result<bool> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let patch = build_hsm_patch(hsm_event, hsm_flags, hsm_error, now);
    let touched = store.patch_sm_status(fid, &patch).await?;
    if !touched {
        tracing::debug!(%fid, event = hsm_event, "HSM event for entry not yet in catalog — skipped");
    } else {
        tracing::debug!(%fid, event = hsm_event, flags = hsm_flags, error = hsm_error, "HSM state patched");
    }
    Ok(touched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_event_sets_archived_state() {
        let p = build_hsm_patch(0, 0, 0, 1700);
        assert_eq!(p["hsm_state"], "archived");
        assert_eq!(p["hsm_last_op"], "archive");
        assert_eq!(p["hsm_dirty"], false);
        assert_eq!(p["hsm_last_event_ts"], 1700);
        assert!(p.get("hsm_last_error").is_none());
    }

    #[test]
    fn release_event_sets_released_state() {
        let p = build_hsm_patch(3, 0, 0, 0);
        assert_eq!(p["hsm_state"], "released");
        assert_eq!(p["hsm_last_op"], "release");
    }

    #[test]
    fn remove_event_sets_none_state() {
        let p = build_hsm_patch(4, 0, 0, 0);
        assert_eq!(p["hsm_state"], "none");
        assert_eq!(p["hsm_last_op"], "remove");
    }

    #[test]
    fn error_records_code_without_state() {
        let p = build_hsm_patch(0, 1, 22, 0);
        assert_eq!(p["hsm_last_op"], "archive");
        assert_eq!(p["hsm_last_error"], 22);
        assert_eq!(p["hsm_dirty"], true);
        assert!(p.get("hsm_state").is_none(), "error must not set state");
    }

    #[test]
    fn state_event_records_op_but_no_state() {
        // HE_STATE = 5 — can't infer state from the event alone.
        let p = build_hsm_patch(5, 0, 0, 0);
        assert_eq!(p["hsm_last_op"], "state");
        assert!(p.get("hsm_state").is_none());
    }
}

/// Re-stat an existing entry and update it in the store.
async fn restat_entry(
    store: &EntryStore, mount: &Path, fid: &lustre_api::LuFid, parent: Option<lustre_api::LuFid>, name: Bytes,
) -> anyhow::Result<bool> {
    if let Some(mut entry) = store.get_entry(fid).await? {
        // Re-stat via FID path for fresh metadata.
        let lustre = LustreApi;
        let fid_copy = *fid;
        let mount_owned = mount.to_owned();
        let stat_result = tokio::task::spawn_blocking(move || {
            let mount_str = mount_owned.to_string_lossy();
            lustre
                .fid_to_path(&mount_str, &fid_copy)
                .ok()
                .map(|rel| mount_owned.join(rel))
                .and_then(|p| std::fs::symlink_metadata(&p).ok())
        })
        .await?;

        if let Some(meta) = stat_result {
            entry.size = meta.size();
            entry.blocks = meta.blocks();
            entry.uid = meta.uid();
            entry.gid = meta.gid();
            entry.mode = meta.mode();
            entry.nlink = meta.nlink() as u32;
            entry.atime = meta.atime();
            entry.mtime = meta.mtime();
            entry.ctime = meta.ctime();
            entry.last_seen = now_secs();
            if !name.is_empty() {
                entry.name = name;
            }
            // Only update parent_fid if non-zero — some record types
            // (e.g. CLOSE, TRUNC) don't populate cr_pfid.
            if let Some(p) = parent
                && !p.is_zero()
            {
                entry.parent_fid = Some(p);
            }
            store.upsert_entry(&entry).await?;
            Ok(true)
        } else {
            // File gone between event and processing.
            Ok(false)
        }
    } else {
        // Not in catalog — skip metadata-only events for unknown entries.
        Ok(false)
    }
}

/// Stat a file by FID and build an EntryRow.
async fn stat_entry_by_fid(
    mount: &Path, fid: &lustre_api::LuFid, parent_fid: Option<lustre_api::LuFid>, name: Bytes,
    _expected_kind: EntryKind,
) -> anyhow::Result<EntryRow> {
    let lustre = LustreApi;
    let fid_copy = *fid;
    let mount_owned = mount.to_owned();

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<EntryRow> {
        let mount_str = mount_owned.to_string_lossy();
        let rel_path = lustre
            .fid_to_path(&mount_str, &fid_copy)
            .map_err(|e| anyhow::anyhow!("fid_to_path: {e}"))?;
        let abs_path = mount_owned.join(&rel_path);

        let meta =
            std::fs::symlink_metadata(&abs_path).map_err(|e| anyhow::anyhow!("stat {}: {e}", abs_path.display()))?;

        let kind = metadata_to_kind(&meta);

        // Stripe info for files.
        let (stripe_count, stripe_size, pool_name) = if kind == EntryKind::File {
            match lustre.get_stripe_info(&abs_path) {
                Ok(info) => (Some(info.count as u16), Some(info.size as u32), info.pool),
                Err(_) => (None, None, None),
            }
        } else {
            (None, None, None)
        };

        // Never store a zero parent_fid — some event types leave cr_pfid unpopulated.
        let safe_parent = parent_fid.filter(|p| !p.is_zero());

        Ok(EntryRow {
            fid: fid_copy,
            parent_fid: safe_parent,
            name,
            kind,
            size: meta.size(),
            blocks: meta.blocks(),
            uid: meta.uid(),
            gid: meta.gid(),
            projid: 0,
            mode: meta.mode(),
            nlink: meta.nlink() as u32,
            atime: meta.atime(),
            mtime: meta.mtime(),
            ctime: meta.ctime(),
            stripe_count,
            stripe_size,
            pool_name,
            sm_status: serde_json::json!({}),
            last_seen: now_secs(),
        })
    })
    .await??;

    Ok(result)
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
        EntryKind::Socket
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Returns `true` when `llapi_fid2path` successfully resolves the FID
/// under `mount`, i.e. the file still exists somewhere on the
/// filesystem. Any FFI error (including ENOENT from a truly removed
/// FID) returns `false`. Used as a DNE-rename hedge — a cross-MDT
/// rename can surface as a last-link UNLNK on the source MDT even
/// though the file lives on another.
async fn fid_still_live(mount: &Path, fid: &lustre_api::LuFid) -> bool {
    let lustre = lustre_api::LustreApi;
    let fid = *fid;
    let mount = mount.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mount_s = mount.to_string_lossy();
        lustre.fid_to_path(&mount_s, &fid).is_ok()
    })
    .await
    .unwrap_or(false)
}
