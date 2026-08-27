//! Backend-neutral POSIX namespace traversal.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_channel::{Receiver, Sender};
use bytes::Bytes;
use rbh_entry_store::model::EntryKind;
use tokio::sync::mpsc;

use crate::walker::{ScanConfig, ScanProgress, glob_matches, load_rbh_ignore_file};

/// POSIX facts gathered without consulting any backend identity API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PosixEntry {
    pub path: PathBuf,
    pub parent_path: Option<PathBuf>,
    pub name: Bytes,
    pub kind: EntryKind,
    pub device: u64,
    pub inode: u64,
    /// Native inode of the containing directory.  Keeping this beside the
    /// object's inode lets backend adapters build a namespace graph without
    /// deriving identity from path text.
    pub parent_inode: Option<u64>,
    pub size: u64,
    pub blocks: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub nlink: u32,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub depth: u32,
}

#[derive(Debug)]
pub enum PosixWalkEvent {
    Entry(Box<PosixEntry>),
    Error { path: String, error: String },
}

pub struct PosixWalker;

impl PosixWalker {
    #[tracing::instrument(skip_all, fields(root = %config.root.display(), concurrency = config.concurrency))]
    pub fn run(config: ScanConfig) -> (mpsc::Receiver<PosixWalkEvent>, Arc<ScanProgress>) {
        let (event_tx, event_rx) = mpsc::channel(config.channel_size);
        let progress = Arc::new(ScanProgress::new());
        let (work_tx, work_rx) = async_channel::unbounded::<(PathBuf, usize)>();
        let pending = Arc::new(AtomicUsize::new(1));
        let _ = work_tx.try_send((config.root.clone(), 0));

        let mut ignore = config.ignore_globs.clone();
        ignore.extend(load_rbh_ignore_file(&config.root));
        let state = Arc::new(WalkState {
            max_depth: config.max_depth,
            work_tx: work_tx.clone(),
            event_tx: event_tx.clone(),
            pending,
            progress: progress.clone(),
            since_mtime: config.since_mtime,
            ignore_globs: Arc::new(ignore),
        });

        for worker_id in 0..config.concurrency.max(1) {
            let work_rx = work_rx.clone();
            let state = state.clone();
            tokio::spawn(async move {
                worker(&state, &work_rx).await;
                tracing::debug!(worker_id, "POSIX walk worker finished");
            });
        }
        drop(work_tx);
        drop(event_tx);
        (event_rx, progress)
    }
}

struct WalkState {
    max_depth: Option<usize>,
    work_tx: Sender<(PathBuf, usize)>,
    event_tx: mpsc::Sender<PosixWalkEvent>,
    pending: Arc<AtomicUsize>,
    progress: Arc<ScanProgress>,
    since_mtime: Option<i64>,
    ignore_globs: Arc<Vec<String>>,
}

async fn worker(state: &WalkState, work_rx: &Receiver<(PathBuf, usize)>) {
    while let Ok((directory, depth)) = work_rx.recv().await {
        process_directory(state, &directory, depth).await;
        if state.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            state.work_tx.close();
        }
    }
}

async fn process_directory(state: &WalkState, directory: &Path, depth: usize) {
    state.progress.dirs_walked.fetch_add(1, Ordering::Relaxed);
    emit_path(state, directory, depth as u32, true).await;

    let mut entries = match tokio::fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) => {
            emit_error(state, directory, format!("readdir failed: {error}")).await;
            return;
        }
    };
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                emit_error(state, directory, format!("readdir failed: {error}")).await;
                break;
            }
        };
        let path = entry.path();
        if state
            .ignore_globs
            .iter()
            .any(|pattern| glob_matches(pattern, &entry.file_name().to_string_lossy()))
        {
            continue;
        }
        match entry.file_type().await {
            Ok(kind) if kind.is_dir() => {
                if state.max_depth.is_none_or(|maximum| depth < maximum) {
                    state.pending.fetch_add(1, Ordering::AcqRel);
                    if state.work_tx.send((path, depth + 1)).await.is_err() {
                        state.pending.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            }
            Ok(_) => emit_path(state, &path, depth as u32 + 1, false).await,
            Err(error) => emit_error(state, &path, format!("file_type failed: {error}")).await,
        }
    }
}

async fn emit_path(state: &WalkState, path: &Path, depth: u32, directory: bool) {
    let owned = path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || stat_entry(&owned, depth)).await;
    match result {
        Ok(Ok(entry)) => {
            if !directory && state.since_mtime.is_some_and(|cutoff| entry.mtime < cutoff) {
                return;
            }
            state.progress.entries_scanned.fetch_add(1, Ordering::Relaxed);
            let _ = state.event_tx.send(PosixWalkEvent::Entry(Box::new(entry))).await;
        }
        Ok(Err(error)) => emit_error(state, path, error.to_string()).await,
        Err(error) => emit_error(state, path, format!("stat task failed: {error}")).await,
    }
}

async fn emit_error(state: &WalkState, path: &Path, error: String) {
    state.progress.errors.fetch_add(1, Ordering::Relaxed);
    let _ = state
        .event_tx
        .send(PosixWalkEvent::Error {
            path: path.display().to_string(),
            error,
        })
        .await;
}

fn stat_entry(path: &Path, depth: u32) -> std::io::Result<PosixEntry> {
    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_file() {
        EntryKind::File
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_char_device() {
        EntryKind::CharDevice
    } else if file_type.is_block_device() {
        EntryKind::BlockDevice
    } else if file_type.is_fifo() {
        EntryKind::Fifo
    } else {
        EntryKind::Socket
    };
    let parent_inode = if depth == 0 {
        None
    } else {
        path.parent()
            .map(std::fs::symlink_metadata)
            .transpose()?
            .map(|parent| parent.ino())
    };
    Ok(PosixEntry {
        path: path.to_path_buf(),
        parent_path: (depth > 0).then(|| path.parent().map(Path::to_path_buf)).flatten(),
        name: path
            .file_name()
            .map(|name| Bytes::copy_from_slice(name.as_encoded_bytes()))
            .unwrap_or_default(),
        kind,
        device: metadata.dev(),
        inode: metadata.ino(),
        parent_inode,
        size: metadata.size(),
        blocks: metadata.blocks(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        mode: metadata.mode(),
        nlink: metadata.nlink() as u32,
        atime: metadata.atime(),
        mtime: metadata.mtime(),
        ctime: metadata.ctime(),
        depth,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn walks_posix_without_lustre_and_preserves_hardlink_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("first"), b"data").unwrap();
        std::fs::hard_link(root.path().join("first"), root.path().join("second")).unwrap();
        let (mut events, _) = PosixWalker::run(ScanConfig {
            root: root.path().to_path_buf(),
            concurrency: 2,
            ..ScanConfig::default()
        });
        let mut files = Vec::new();
        while let Some(event) = events.recv().await {
            if let PosixWalkEvent::Entry(entry) = event
                && entry.kind == EntryKind::File
            {
                files.push(*entry);
            }
        }
        assert!(files.iter().all(|entry| entry.parent_inode.is_some()));
        assert_eq!(files.len(), 2);
        assert_eq!((files[0].device, files[0].inode), (files[1].device, files[1].inode));
    }

    #[tokio::test]
    async fn ignored_directory_is_pruned_and_rescan_is_stable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/hidden"), b"data").unwrap();
        std::fs::write(root.path().join("visible"), b"data").unwrap();
        let scan = || {
            PosixWalker::run(ScanConfig {
                root: root.path().to_path_buf(),
                concurrency: 1,
                ignore_globs: vec!["ignored".into()],
                ..ScanConfig::default()
            })
            .0
        };
        let mut snapshots = Vec::new();
        for _ in 0..2 {
            let mut events = scan();
            let mut names = Vec::new();
            while let Some(event) = events.recv().await {
                if let PosixWalkEvent::Entry(entry) = event {
                    names.push(entry.name.clone());
                }
            }
            names.sort();
            snapshots.push(names);
        }
        assert_eq!(snapshots[0], snapshots[1]);
        assert!(snapshots[0].contains(&Bytes::from_static(b"visible")));
        assert!(!snapshots[0].contains(&Bytes::from_static(b"hidden")));
    }
}
