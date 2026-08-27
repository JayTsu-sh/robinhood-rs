#![allow(clippy::items_after_test_module)]

//! Changelog ingest — consumes [`EventBatch`]es from the changelog listener
//! and applies them to the entry store.
//!
//! For creation events (Create, Mkdir, Mknod, Softlink), we stat the file
//! via `fid_to_path` + `symlink_metadata` to populate the full [`EntryRow`].
//! For deletion events (Unlink with last_link, Rmdir), we call `legacy_lustre_remove_entry`.
//! For Rename, we update parent_fid + name and handle rename-overwrite.
//! For metadata events (Close, SetAttr, Truncate, MTime, CTime), we re-stat
//! the file to refresh changed fields.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio_util::sync::CancellationToken;

use lustre_api::LustreApi;
use lustre_changelog::ChangelogEvent;
use rbh_change_source::{BackendChange, Change, ChangeSource, ContentChangeKind, MetadataChangeKind};
use rbh_entry_store::model::{
    EntryKey, EntryKind, EntryRow, FileSystemId, ObjectId, ScopedEntryRow, ScopedNamespaceEdge,
};
use rbh_entry_store::store::EntryStore;

const CATCHUP_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestExit {
    Stopped,
    RetentionGap(String),
    BaselineInvalid(String),
}

enum Receive {
    Batch(rbh_change_source::ChangeBatch),
    Closed,
    Failed(rbh_change_source::ChangeSourceError),
    Cancelled,
    Quiescent,
}

async fn receive(source: &mut dyn ChangeSource, cancel: &CancellationToken, wait_for_catchup: bool) -> Receive {
    let next = async {
        match source.next_batch().await {
            Ok(Some(batch)) => Receive::Batch(batch),
            Ok(None) => Receive::Closed,
            Err(error) => Receive::Failed(error),
        }
    };
    if wait_for_catchup {
        tokio::select! {
            result = tokio::time::timeout(CATCHUP_QUIET_PERIOD, next) => {
                result.unwrap_or(Receive::Quiescent)
            }
            _ = cancel.cancelled() => Receive::Cancelled,
        }
    } else {
        tokio::select! {
            result = next => result,
            _ = cancel.cancelled() => Receive::Cancelled,
        }
    }
}

/// Run the changelog ingest loop. Consumes batches from the listener,
/// applies events to the entry store, and sends acks back.
///
/// Exits when the listener channel closes or the cancel token fires.
#[tracing::instrument(name = "changelog.ingest", skip_all, fields(filesystem = %filesystem_id))]
pub async fn ingest_loop(
    mut source: Box<dyn ChangeSource>, entry_store: EntryStore, filesystem_id: FileSystemId, mount_path: PathBuf,
    cancel: CancellationToken, classifier_cache: std::sync::Arc<tokio::sync::RwLock<Vec<rbh_policy::ClassifierRow>>>,
) -> IngestExit {
    tracing::info!("changelog ingest loop started");
    let mut catching_up = entry_store
        .get_baseline(&filesystem_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|baseline| baseline.state == rbh_entry_store::model::BaselineState::CatchingUp);
    let mut last_committed = None;
    let mut verified_quiescent = false;

    loop {
        let batch = match receive(source.as_mut(), &cancel, catching_up).await {
            Receive::Batch(batch) => {
                verified_quiescent = false;
                batch
            }
            Receive::Closed => {
                tracing::info!("changelog event channel closed");
                break;
            }
            Receive::Failed(rbh_change_source::ChangeSourceError::RetentionGap(reason)) => {
                tracing::error!(%reason, "change source retention gap");
                return IngestExit::RetentionGap(reason);
            }
            Receive::Failed(error) => {
                tracing::error!(%error, "change source failed");
                break;
            }
            Receive::Cancelled => {
                tracing::info!("changelog ingest cancelled");
                break;
            }
            Receive::Quiescent => {
                if !verified_quiescent {
                    if let Err(reason) = verify_juicefs_namespace(&entry_store, &filesystem_id, &mount_path).await {
                        let reason = reason.to_string();
                        let _ = entry_store
                            .set_baseline_state(
                                &filesystem_id,
                                rbh_entry_store::model::BaselineState::Invalid,
                                last_committed,
                                Some(&reason),
                            )
                            .await;
                        return IngestExit::BaselineInvalid(reason);
                    }
                    // Require another quiet boundary after the independent
                    // walk so changes that arrived during comparison are
                    // drained before Ready is published.
                    verified_quiescent = true;
                    continue;
                }
                if let Err(error) = entry_store
                    .set_baseline_state(
                        &filesystem_id,
                        rbh_entry_store::model::BaselineState::Ready,
                        last_committed,
                        None,
                    )
                    .await
                {
                    tracing::warn!(%error, "failed to publish JuiceFS baseline completion");
                    break;
                }
                catching_up = false;
                tracing::info!(filesystem = %filesystem_id, ?last_committed, "JuiceFS baseline caught up");
                continue;
            }
        };

        let mdt = batch.checkpoint.source.clone();
        let max_index = batch.checkpoint.position;
        let event_count = batch.changes.len();

        if batch.filesystem != filesystem_id {
            tracing::error!(
                expected = %filesystem_id,
                actual = %batch.filesystem,
                "change batch routed to the wrong filesystem; checkpoint will not advance"
            );
            break;
        }

        tracing::info!(
            mdt = %mdt,
            events = event_count,
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
            for change in &batch.changes {
                *by_type.entry(change.kind_name()).or_insert(0) += 1;
            }
            for (ty, n) in by_type {
                rbh_observability::metrics::CHANGELOG_EVENTS
                    .with_label_values(&[mdt.as_str(), ty])
                    .inc_by(n);
            }
        }

        for change in &batch.changes {
            if change_object(change).backend() == rbh_entry_store::BackendKind::JuiceFs {
                match apply_juicefs_change(&entry_store, &filesystem_id, &mount_path, change).await {
                    Ok(()) => applied += 1,
                    Err(error) => {
                        errors += 1;
                        tracing::error!(%error, "failed to apply JuiceFS catalog change");
                        break;
                    }
                }
                continue;
            }
            let event = match lustre_event(change) {
                Ok(event) => event,
                Err(error) => {
                    errors += 1;
                    tracing::error!(%error, "change is incompatible with Lustre runtime");
                    break;
                }
            };
            let event_time = change.time();
            match apply_event(&entry_store, &filesystem_id, &mount_path, &event, event_time).await {
                Ok(true) => {
                    applied += 1;
                    // Incremental classification: re-classify the affected entry.
                    let fid = event.fid();
                    let classifiers = classifier_cache.read().await;
                    if !classifiers.is_empty() {
                        match entry_store.get_lustre_entry(&filesystem_id, &fid).await {
                            Ok(Some(row)) => {
                                if let Err(error) =
                                    apply_classifiers(&classifiers, &row, &entry_store, &filesystem_id).await
                                {
                                    errors += 1;
                                    tracing::warn!(%fid, %error, "incremental classification failed");
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                errors += 1;
                                tracing::warn!(%fid, %error, "failed to reload entry for classification");
                                break;
                            }
                        }
                    }
                }
                Ok(false) => skipped += 1,
                Err(e) => {
                    errors += 1;
                    tracing::warn!(
                        fid = %event.fid(),
                        kind = event.kind_name(),
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

        if errors > 0 {
            tracing::error!(errors, "batch was not fully applied; checkpoint will not advance");
            break;
        }

        // Commit only after every catalog effect in the batch is durable.
        if let Err(error) = source.commit(batch.checkpoint).await {
            tracing::warn!(%error, "checkpoint commit failed — batch will replay");
            break;
        }
        last_committed = Some(max_index);
    }

    tracing::info!("changelog ingest loop exiting");
    IngestExit::Stopped
}

async fn verify_juicefs_namespace(store: &EntryStore, filesystem: &FileSystemId, mount: &Path) -> anyhow::Result<()> {
    let config = rbh_fs_scan::ScanConfig {
        root: mount.to_path_buf(),
        concurrency: 4,
        max_depth: None,
        channel_size: 1024,
        since_mtime: None,
        ignore_globs: Vec::new(),
    };
    let (mut events, _) = rbh_fs_scan::PosixWalker::run(config);
    let mut mounted = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            rbh_fs_scan::PosixWalkEvent::Entry(entry) => {
                if let Some(edge) = rbh_fs_scan::juicefs::adapt(filesystem, &entry, 0)
                    .map_err(anyhow::Error::msg)?
                    .1
                {
                    mounted.push(edge);
                }
            }
            rbh_fs_scan::PosixWalkEvent::Error { path, error } => {
                anyhow::bail!("namespace verification failed at {path}: {error}")
            }
        }
    }
    let catalog = store.list_scoped_namespace_edges(filesystem).await?;
    rbh_fs_scan::juicefs::compare_namespace(&mounted, &catalog).map_err(|difference| {
        anyhow::anyhow!(
            "namespace mismatch: {} missing from catalog, {} missing from mount",
            difference.missing_from_catalog,
            difference.missing_from_mount
        )
    })
}

fn change_object(change: &Change) -> ObjectId {
    match change {
        Change::Created { object, .. }
        | Change::Hardlinked { object, .. }
        | Change::Removed { object, .. }
        | Change::Renamed { object, .. }
        | Change::MetadataChanged { object, .. }
        | Change::ContentChanged { object, .. }
        | Change::Backend(BackendChange::LustreHsm { object, .. })
        | Change::Backend(BackendChange::LustreLayout { object, .. }) => *object,
    }
}

async fn apply_juicefs_change(
    store: &EntryStore, filesystem: &FileSystemId, mount: &Path, change: &Change,
) -> anyhow::Result<()> {
    let object = change_object(change);
    let key = EntryKey::new(filesystem.clone(), object);
    match change {
        Change::Created {
            parent,
            name,
            kind,
            metadata,
            time,
            ..
        } => {
            let parent_key = EntryKey::new(filesystem.clone(), *parent);
            let path = juicefs_child_path(store, mount, &parent_key, name).await?;
            let live = tokio::fs::symlink_metadata(&path).await.ok();
            if let Some(live) = &live
                && live.ino() != object_inode(object)?
            {
                anyhow::bail!("JuiceFS inode changed before CREATE apply: {}", path.display());
            }
            let (uid, gid, mode) = live
                .as_ref()
                .map(|value| (value.uid(), value.gid(), value.mode()))
                .or_else(|| metadata.map(|value| (value.uid, value.gid, value.mode)))
                .ok_or_else(|| anyhow::anyhow!("CREATE has neither live nor recorded metadata"))?;
            store
                .upsert_scoped_entry(&ScopedEntryRow {
                    key,
                    parent: Some(parent_key),
                    name: name.clone(),
                    kind: *kind,
                    size: live.as_ref().map_or(0, |value| value.size()),
                    blocks: live.as_ref().map_or(0, |value| value.blocks()),
                    uid,
                    gid,
                    projid: 0,
                    mode,
                    nlink: live.as_ref().map_or(1, |value| value.nlink() as u32),
                    atime: live.as_ref().map_or(*time, |value| value.atime()),
                    mtime: live.as_ref().map_or(*time, |value| value.mtime()),
                    ctime: live.as_ref().map_or(*time, |value| value.ctime()),
                    stripe_count: None,
                    stripe_size: None,
                    stripe_items: Vec::new(),
                    pool_name: None,
                    sm_status: serde_json::Value::Null,
                    last_seen: *time,
                    depth: path
                        .strip_prefix(mount)
                        .map_or(0, |value| value.components().count() as u32),
                })
                .await?;
            store
                .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
                    filesystem: filesystem.clone(),
                    parent: *parent,
                    name: name.clone(),
                    object,
                })
                .await?;
            Ok(())
        }
        Change::Renamed {
            source_parent,
            source_name,
            parent,
            name,
            time,
            ..
        } => {
            let mut entry = store
                .get_scoped_entry(&key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("MOVE references an uncataloged inode"))?;
            entry.parent = Some(EntryKey::new(filesystem.clone(), *parent));
            entry.name = name.clone();
            entry.ctime = *time;
            entry.last_seen = *time;
            store.upsert_scoped_entry(&entry).await?;
            store
                .remove_scoped_namespace_edge(filesystem, *source_parent, source_name)
                .await?;
            store
                .upsert_scoped_namespace_edge(&ScopedNamespaceEdge {
                    filesystem: filesystem.clone(),
                    parent: *parent,
                    name: name.clone(),
                    object,
                })
                .await?;
            Ok(())
        }
        Change::Removed {
            parent,
            name,
            directory,
            ..
        } => {
            store
                .apply_scoped_unlink(&key, *parent, name, change.time(), *directory)
                .await?;
            Ok(())
        }
        Change::MetadataChanged { .. } | Change::ContentChanged { .. } => {
            let mut entry = store
                .get_scoped_entry(&key)
                .await?
                .ok_or_else(|| anyhow::anyhow!("change references an uncataloged inode"))?;
            let path = juicefs_object_path(store, mount, &key).await?;
            let live = tokio::fs::symlink_metadata(&path).await?;
            if live.ino() != object_inode(object)? {
                anyhow::bail!("JuiceFS inode changed before metadata apply: {}", path.display());
            }
            entry.size = live.size();
            entry.blocks = live.blocks();
            entry.uid = live.uid();
            entry.gid = live.gid();
            entry.mode = live.mode();
            entry.nlink = live.nlink() as u32;
            entry.atime = live.atime();
            entry.mtime = live.mtime();
            entry.ctime = live.ctime();
            entry.last_seen = change.time();
            store.upsert_scoped_entry(&entry).await?;
            Ok(())
        }
        Change::Hardlinked { parent, name, time, .. } => {
            store
                .apply_scoped_hardlink(
                    &ScopedNamespaceEdge {
                        filesystem: filesystem.clone(),
                        parent: *parent,
                        name: name.clone(),
                        object,
                    },
                    *time,
                )
                .await?;
            Ok(())
        }
        Change::Backend(_) => anyhow::bail!("backend event cannot belong to JuiceFS"),
    }
}

fn object_inode(object: ObjectId) -> anyhow::Result<u64> {
    match object {
        ObjectId::JuiceFs(inode) => Ok(inode),
        ObjectId::Lustre(_) => anyhow::bail!("expected JuiceFS inode"),
    }
}

async fn juicefs_child_path(
    store: &EntryStore, mount: &Path, parent: &EntryKey, name: &Bytes,
) -> anyhow::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Ok(juicefs_object_path(store, mount, parent)
        .await?
        .join(std::ffi::OsString::from_vec(name.to_vec())))
}

async fn juicefs_object_path(store: &EntryStore, mount: &Path, key: &EntryKey) -> anyhow::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    let mut current = key.clone();
    let mut names: Vec<Vec<u8>> = Vec::new();
    for _ in 0..1024 {
        if current.object() == &ObjectId::JuiceFs(1) {
            let mut path = mount.to_path_buf();
            for name in names.iter().rev() {
                path.push(std::ffi::OsString::from_vec(name.clone()));
            }
            return Ok(path);
        }
        let entry = store
            .get_scoped_entry(&current)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing JuiceFS namespace parent"))?;
        names.push(entry.name.to_vec());
        current = entry
            .parent
            .ok_or_else(|| anyhow::anyhow!("JuiceFS namespace chain has no root"))?;
    }
    anyhow::bail!("JuiceFS namespace chain exceeds 1024 parents")
}

/// Run all enabled classifiers against a single entry in-memory and write back any tags.
async fn apply_classifiers(
    classifiers: &[rbh_policy::ClassifierRow], entry: &rbh_entry_store::model::EntryRow,
    store: &rbh_entry_store::store::EntryStore, filesystem: &FileSystemId,
) -> anyhow::Result<()> {
    for classifier in classifiers {
        if !classifier.enabled {
            continue;
        }
        if let Some(tags) = rbh_policy::evaluate_classifier(&classifier.definition, entry) {
            store
                .legacy_lustre_update_xattr(&entry.fid, tags, &classifier.definition.manages)
                .await?;
            store
                .update_scoped_xattr(
                    &EntryKey::new(filesystem.clone(), ObjectId::Lustre(entry.fid)),
                    tags,
                    &classifier.definition.manages,
                )
                .await?;
        }
    }
    Ok(())
}

fn lustre_fid(object: ObjectId) -> anyhow::Result<lustre_api::LuFid> {
    match object {
        ObjectId::Lustre(fid) => Ok(fid),
        ObjectId::JuiceFs(inode) => anyhow::bail!("JuiceFS inode {inode} received by Lustre runtime"),
    }
}

fn lustre_event(change: &Change) -> anyhow::Result<ChangelogEvent> {
    Ok(match change {
        Change::Created {
            object,
            parent,
            name,
            kind,
            ..
        } => {
            let fid = lustre_fid(*object)?;
            let parent = lustre_fid(*parent)?;
            match kind {
                EntryKind::Directory => ChangelogEvent::Mkdir {
                    fid,
                    parent,
                    name: name.clone(),
                    jobid: None,
                    uid: None,
                    gid: None,
                },
                EntryKind::Symlink => ChangelogEvent::Softlink {
                    fid,
                    parent,
                    name: name.clone(),
                },
                _ => ChangelogEvent::Create {
                    fid,
                    parent,
                    name: name.clone(),
                    jobid: None,
                    uid: None,
                    gid: None,
                },
            }
        }
        Change::Hardlinked {
            object, parent, name, ..
        } => ChangelogEvent::Hardlink {
            fid: lustre_fid(*object)?,
            parent: lustre_fid(*parent)?,
            name: name.clone(),
        },
        Change::Removed {
            object,
            parent,
            name,
            last_link,
            directory,
            ..
        } => {
            let fid = lustre_fid(*object)?;
            if *directory {
                ChangelogEvent::Rmdir {
                    fid,
                    parent: lustre_fid(*parent)?,
                    name: name.clone(),
                }
            } else {
                ChangelogEvent::Unlink {
                    fid,
                    parent: lustre_fid(*parent)?,
                    name: name.clone(),
                    last_link: *last_link,
                    hsm_exists: false,
                }
            }
        }
        Change::Renamed {
            object,
            source_parent,
            source_name,
            parent,
            name,
            ..
        } => ChangelogEvent::Rename {
            fid: lustre_fid(*object)?,
            parent: lustre_fid(*parent)?,
            name: name.clone(),
            src_parent: lustre_fid(*source_parent)?,
            src_name: source_name.clone(),
        },
        Change::MetadataChanged { object, kind, .. } => match kind {
            MetadataChangeKind::Attributes => ChangelogEvent::SetAttr {
                fid: lustre_fid(*object)?,
            },
            MetadataChangeKind::Xattr => ChangelogEvent::XAttr {
                fid: lustre_fid(*object)?,
                xattr_name: Bytes::new(),
            },
        },
        Change::ContentChanged {
            object,
            parent,
            name,
            kind,
            ..
        } => match kind {
            ContentChangeKind::Data => ChangelogEvent::Close {
                fid: lustre_fid(*object)?,
                parent: parent.map(lustre_fid).transpose()?.unwrap_or_default(),
                name: name.clone(),
            },
            ContentChangeKind::Truncate => ChangelogEvent::Truncate {
                fid: lustre_fid(*object)?,
            },
        },
        Change::Backend(BackendChange::LustreHsm {
            object,
            event,
            flags,
            error,
            ..
        }) => ChangelogEvent::Hsm {
            fid: lustre_fid(*object)?,
            hsm_event: *event,
            hsm_flags: *flags,
            hsm_error: *error,
        },
        Change::Backend(BackendChange::LustreLayout { object, .. }) => ChangelogEvent::Layout {
            fid: lustre_fid(*object)?,
        },
    })
}

/// Apply a single changelog event to the entry store.
/// Returns `Ok(true)` if the store was modified, `Ok(false)` if skipped.
async fn apply_event(
    store: &EntryStore, filesystem: &FileSystemId, mount: &Path, event: &ChangelogEvent, event_time: i64,
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
                    store.upsert_lustre_entry(filesystem, &entry).await?;
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
                    store.upsert_lustre_entry(filesystem, &entry).await?;
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }

        ChangelogEvent::Hardlink { fid, parent, name } => {
            // Update the entry's parent/name to the new link location.
            // A full hardlink implementation would insert into the names table.
            if let Some(mut entry) = store.get_lustre_entry(filesystem, fid).await? {
                entry.parent_fid = Some(*parent);
                entry.name = name.clone();
                entry.nlink = entry.nlink.saturating_add(1);
                entry.last_seen = now_secs();
                store.upsert_lustre_entry(filesystem, &entry).await?;
                Ok(true)
            } else {
                // Entry not in catalog — try stat
                match stat_entry_by_fid(mount, fid, Some(*parent), name.clone(), EntryKind::File).await {
                    Ok(entry) => {
                        store.upsert_lustre_entry(filesystem, &entry).await?;
                        Ok(true)
                    }
                    Err(_) => Ok(false),
                }
            }
        }

        // ── Deletion events ──
        ChangelogEvent::Unlink {
            fid,
            parent,
            name,
            last_link,
            ..
        } => {
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
                store.remove_lustre_entry(filesystem, fid, event_time).await?;
                Ok(true)
            } else {
                store
                    .remove_scoped_namespace_edge(filesystem, ObjectId::Lustre(*parent), name)
                    .await?;
                // Decrement nlink if the entry exists.
                if let Some(mut entry) = store.get_lustre_entry(filesystem, fid).await? {
                    entry.nlink = entry.nlink.saturating_sub(1);
                    entry.last_seen = now_secs();
                    store.upsert_lustre_entry(filesystem, &entry).await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }

        ChangelogEvent::Rmdir { fid, .. } => {
            store.remove_lustre_entry(filesystem, fid, event_time).await?;
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
            let existing = store.get_lustre_entry(filesystem, fid).await?;
            if let Some(mut entry) = existing {
                entry.parent_fid = Some(*parent);
                entry.name = name.clone();
                entry.last_seen = now_secs();
                store.rename_lustre_entry(filesystem, &entry, event_time).await?;
                Ok(true)
            } else {
                match stat_entry_by_fid(mount, fid, Some(*parent), name.clone(), EntryKind::File).await {
                    Ok(entry) => {
                        store.rename_lustre_entry(filesystem, &entry, event_time).await?;
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
            restat_entry(store, filesystem, mount, fid, parent_opt, name.clone()).await
        }

        ChangelogEvent::Truncate { fid } => {
            // TRUNC is how HSM release manifests in the changelog on this
            // Lustre version — a CL_HSM(RELEASE) event is NOT generated.
            // After re-statting for size/mtime, check the live HSM state
            // and refresh sm_status.hsm_state if the entry is HSM-managed.
            let touched = restat_entry(store, filesystem, mount, fid, None, Bytes::new()).await?;
            if touched {
                let _ = refresh_hsm_state_if_managed(store, filesystem, mount, fid).await;
            }
            Ok(touched)
        }

        ChangelogEvent::SetAttr { fid } | ChangelogEvent::MTime { fid } | ChangelogEvent::CTime { fid } => {
            restat_entry(store, filesystem, mount, fid, None, Bytes::new()).await
        }

        ChangelogEvent::Hsm {
            fid,
            hsm_event,
            hsm_flags,
            hsm_error,
        } => apply_hsm_event(store, filesystem, fid, *hsm_event, *hsm_flags, *hsm_error).await,

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
    store: &EntryStore, filesystem: &FileSystemId, fid: &lustre_api::LuFid, hsm_event: u8, hsm_flags: u8, hsm_error: u8,
) -> anyhow::Result<bool> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let patch = build_hsm_patch(hsm_event, hsm_flags, hsm_error, now);
    let touched = store.legacy_lustre_patch_sm_status(fid, &patch).await?;
    let scoped_touched = store
        .patch_scoped_sm_status(&EntryKey::new(filesystem.clone(), ObjectId::Lustre(*fid)), &patch)
        .await?;
    if !touched && !scoped_touched {
        tracing::debug!(%fid, event = hsm_event, "HSM event for entry not yet in catalog — skipped");
    } else {
        tracing::debug!(%fid, event = hsm_event, flags = hsm_flags, error = hsm_error, "HSM state patched");
    }
    Ok(touched || scoped_touched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rbh_change_source::{ChangeBatch, ChangeSourceError, Checkpoint};
    use rbh_entry_store::{BackendCapabilities, BackendKind, FileSystemConfig};
    use tokio::sync::oneshot;

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

    struct OneBatchSource {
        batch: Option<ChangeBatch>,
        committed: Option<oneshot::Sender<Checkpoint>>,
    }

    struct RetentionGapSource;

    #[async_trait]
    impl ChangeSource for RetentionGapSource {
        async fn next_batch(&mut self) -> Result<Option<ChangeBatch>, ChangeSourceError> {
            Err(ChangeSourceError::RetentionGap("cursor expired".into()))
        }

        async fn commit(&mut self, _: Checkpoint) -> Result<(), ChangeSourceError> {
            unreachable!()
        }
    }

    struct IdleSource;

    #[async_trait]
    impl ChangeSource for IdleSource {
        async fn next_batch(&mut self) -> Result<Option<ChangeBatch>, ChangeSourceError> {
            std::future::pending().await
        }

        async fn commit(&mut self, _: Checkpoint) -> Result<(), ChangeSourceError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn catchup_reaches_a_quiescent_boundary_without_a_new_record() {
        let mut source = IdleSource;
        assert!(matches!(
            receive(&mut source, &CancellationToken::new(), true).await,
            Receive::Quiescent
        ));
    }

    #[tokio::test]
    async fn retention_gap_is_exposed_to_the_runtime_without_ack() {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://root@localhost/unused")
            .unwrap();
        let exit = ingest_loop(
            Box::new(RetentionGapSource),
            EntryStore::with_pool(pool),
            FileSystemId::new("juice").unwrap(),
            "/jfs".into(),
            CancellationToken::new(),
            Default::default(),
        )
        .await;
        assert_eq!(exit, IngestExit::RetentionGap("cursor expired".into()));
    }

    #[async_trait]
    impl ChangeSource for OneBatchSource {
        async fn next_batch(&mut self) -> Result<Option<ChangeBatch>, ChangeSourceError> {
            Ok(self.batch.take())
        }

        async fn commit(&mut self, checkpoint: Checkpoint) -> Result<(), ChangeSourceError> {
            if let Some(committed) = self.committed.take() {
                let _ = committed.send(checkpoint);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn juicefs_checkpoint_is_committed_after_catalog_write() {
        if std::env::var("RBH_INTEGRATION").as_deref() != Ok("1") {
            return;
        }
        let url = "mysql://root@localhost/rbh_entries_test";
        let store = EntryStore::connect(url).await.unwrap();
        let filesystem = FileSystemId::new("jfs-ack-test").unwrap();
        store
            .register_filesystem(&FileSystemConfig {
                id: filesystem.clone(),
                backend: BackendKind::JuiceFs,
                mount_path: "/mnt/jfs".into(),
                capabilities: BackendCapabilities {
                    changelog: true,
                    namespace: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        store.clear_scoped_catalog(&filesystem).await.unwrap();
        let checkpoint = Checkpoint {
            source: "jfs-nfs".into(),
            position: 77,
        };
        let (commit_tx, commit_rx) = oneshot::channel();
        ingest_loop(
            Box::new(OneBatchSource {
                batch: Some(ChangeBatch {
                    filesystem: filesystem.clone(),
                    changes: vec![Change::Created {
                        object: ObjectId::JuiceFs(42),
                        parent: ObjectId::JuiceFs(1),
                        name: Bytes::from_static(b"durable-before-ack"),
                        kind: EntryKind::File,
                        metadata: Some(rbh_change_source::CreatedMetadata {
                            uid: 1000,
                            gid: 1000,
                            mode: 0o644,
                        }),
                        time: 1_700_000_000,
                    }],
                    checkpoint: checkpoint.clone(),
                }),
                committed: Some(commit_tx),
            }),
            store.clone(),
            filesystem.clone(),
            "/mnt/jfs".into(),
            CancellationToken::new(),
            Default::default(),
        )
        .await;
        assert_eq!(commit_rx.await.unwrap(), checkpoint);
        let persisted = store
            .get_scoped_entry(&EntryKey::new(filesystem, ObjectId::JuiceFs(42)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.name, Bytes::from_static(b"durable-before-ack"));

        let link = Change::Hardlinked {
            object: ObjectId::JuiceFs(42),
            parent: ObjectId::JuiceFs(1),
            name: Bytes::from_static(b"alias"),
            time: 1_700_000_002,
        };
        apply_juicefs_change(&store, persisted.key.filesystem(), Path::new("/mnt/jfs"), &link)
            .await
            .unwrap();
        apply_juicefs_change(&store, persisted.key.filesystem(), Path::new("/mnt/jfs"), &link)
            .await
            .unwrap();
        let replayed = store.get_scoped_entry(&persisted.key).await.unwrap().unwrap();
        assert_eq!(replayed.nlink, 2, "replayed LINK must not increment nlink twice");
        let unlink = Change::Removed {
            object: ObjectId::JuiceFs(42),
            parent: ObjectId::JuiceFs(1),
            name: Bytes::from_static(b"alias"),
            last_link: false,
            directory: false,
            time: 1_700_000_003,
        };
        apply_juicefs_change(&store, persisted.key.filesystem(), Path::new("/mnt/jfs"), &unlink)
            .await
            .unwrap();
        apply_juicefs_change(&store, persisted.key.filesystem(), Path::new("/mnt/jfs"), &unlink)
            .await
            .unwrap();
        let replayed = store.get_scoped_entry(&persisted.key).await.unwrap().unwrap();
        assert_eq!(replayed.nlink, 1, "replayed UNLINK must not decrement nlink twice");

        let rejected_filesystem = FileSystemId::new("jfs-rejected-test").unwrap();
        store
            .register_filesystem(&FileSystemConfig {
                id: rejected_filesystem.clone(),
                backend: BackendKind::Lustre,
                mount_path: "/mnt/lustre".into(),
                capabilities: BackendCapabilities::default(),
            })
            .await
            .unwrap();
        let (should_not_commit, commit_rx) = oneshot::channel();
        ingest_loop(
            Box::new(OneBatchSource {
                batch: Some(ChangeBatch {
                    filesystem: rejected_filesystem.clone(),
                    changes: vec![Change::Created {
                        object: ObjectId::JuiceFs(99),
                        parent: ObjectId::JuiceFs(1),
                        name: Bytes::from_static(b"must-not-ack"),
                        kind: EntryKind::File,
                        metadata: Some(rbh_change_source::CreatedMetadata {
                            uid: 1000,
                            gid: 1000,
                            mode: 0o644,
                        }),
                        time: 1_700_000_001,
                    }],
                    checkpoint: Checkpoint {
                        source: "jfs-nfs".into(),
                        position: 78,
                    },
                }),
                committed: Some(should_not_commit),
            }),
            store,
            rejected_filesystem,
            "/mnt/jfs".into(),
            CancellationToken::new(),
            Default::default(),
        )
        .await;
        assert!(commit_rx.await.is_err(), "failed catalog write must not commit or ACK");
    }
}

/// Re-stat an existing entry and update it in the store.
/// Query the live HSM state via FFI and update sm_status.hsm_state in the catalog.
/// Only called when the entry already has an hsm_state (meaning it's HSM-managed).
/// This handles the case where TRUNC events are generated instead of CL_HSM(RELEASE).
async fn refresh_hsm_state_if_managed(
    store: &EntryStore, filesystem: &FileSystemId, mount: &Path, fid: &lustre_api::LuFid,
) -> anyhow::Result<()> {
    // Check if entry has an existing hsm_state in the catalog.
    let entry = match store.get_lustre_entry(filesystem, fid).await? {
        Some(e) => e,
        None => return Ok(()),
    };
    let current_state = entry.sm_status.get("hsm_state").and_then(|v| v.as_str()).unwrap_or("");
    if current_state.is_empty() || current_state == "none" {
        return Ok(()); // not HSM-managed, skip
    }

    // Query live HSM state via llapi_hsm_state_get.
    let lustre = LustreApi;
    let fid_copy = *fid;
    let mount_owned = mount.to_owned();
    let hsm_info = tokio::task::spawn_blocking(move || {
        let mount_str = mount_owned.to_string_lossy();
        lustre
            .fid_to_path(&mount_str, &fid_copy)
            .ok()
            .map(|rel| mount_owned.join(rel))
            .and_then(|p| lustre.hsm_state_get(&p).ok())
    })
    .await
    .unwrap_or(None);

    let Some(info) = hsm_info else {
        return Ok(());
    };

    use lustre_api::HsmState;
    let new_state = if info.states.contains(HsmState::RELEASED) {
        "released"
    } else if info.states.contains(HsmState::ARCHIVED) && !info.states.contains(HsmState::DIRTY) {
        "archived"
    } else if info.states.is_empty() {
        "none"
    } else {
        return Ok(()); // dirty, exists-only, etc. — leave existing state
    };

    if new_state != current_state {
        tracing::debug!(
            %fid,
            old_state = %current_state,
            new_state,
            "HSM state refreshed after TRUNC event"
        );
        let patch = serde_json::json!({ "hsm_state": new_state });
        if let Err(e) = store.legacy_lustre_patch_sm_status(fid, &patch).await {
            tracing::warn!(%fid, error = %e, new_state, "failed to patch hsm_state after TRUNC event");
        }
        store
            .patch_scoped_sm_status(&EntryKey::new(filesystem.clone(), ObjectId::Lustre(*fid)), &patch)
            .await?;
    }
    Ok(())
}

async fn restat_entry(
    store: &EntryStore, filesystem: &FileSystemId, mount: &Path, fid: &lustre_api::LuFid,
    parent: Option<lustre_api::LuFid>, name: Bytes,
) -> anyhow::Result<bool> {
    if let Some(mut entry) = store.get_lustre_entry(filesystem, fid).await? {
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
            store.upsert_lustre_entry(filesystem, &entry).await?;
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
        let (stripe_count, stripe_size, stripe_items, pool_name) = if kind == EntryKind::File {
            match lustre.get_stripe_info(&abs_path) {
                Ok(info) => (
                    Some(info.count as u16),
                    Some(info.size as u32),
                    info.ost_indices,
                    info.pool,
                ),
                Err(_) => (None, None, Vec::new(), None),
            }
        } else {
            (None, None, Vec::new(), None)
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
            stripe_items,
            pool_name,
            sm_status: serde_json::json!({}),
            last_seen: now_secs(),
            depth: 0,
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
