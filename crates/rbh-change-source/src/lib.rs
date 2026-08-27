//! Backend-neutral filesystem change stream seam.

use async_trait::async_trait;
use bytes::Bytes;
use lustre_changelog::{ChangelogEvent, EventAck, ListenerHandle};
use rbh_entry_store::model::{EntryKind, FileSystemId, ObjectId};
use std::collections::VecDeque;

mod juicefs;
pub use juicefs::JuiceFsChangeSource;

pub mod juicefs_proto {
    tonic::include_proto!("robinhood.juicefs.v1");
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub source: String,
    pub position: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeBatch {
    pub filesystem: FileSystemId,
    pub changes: Vec<Change>,
    pub checkpoint: Checkpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created {
        object: ObjectId,
        parent: ObjectId,
        name: Bytes,
        kind: EntryKind,
        metadata: Option<CreatedMetadata>,
        time: i64,
    },
    Hardlinked {
        object: ObjectId,
        parent: ObjectId,
        name: Bytes,
        time: i64,
    },
    Removed {
        object: ObjectId,
        parent: ObjectId,
        name: Bytes,
        last_link: bool,
        directory: bool,
        time: i64,
    },
    Renamed {
        object: ObjectId,
        source_parent: ObjectId,
        source_name: Bytes,
        parent: ObjectId,
        name: Bytes,
        time: i64,
    },
    MetadataChanged {
        object: ObjectId,
        kind: MetadataChangeKind,
        time: i64,
    },
    ContentChanged {
        object: ObjectId,
        parent: Option<ObjectId>,
        name: Bytes,
        kind: ContentChangeKind,
        time: i64,
    },
    Backend(BackendChange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreatedMetadata {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentChangeKind {
    Data,
    Truncate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataChangeKind {
    Attributes,
    Xattr,
}

impl Change {
    pub fn time(&self) -> i64 {
        match self {
            Self::Created { time, .. }
            | Self::Hardlinked { time, .. }
            | Self::Removed { time, .. }
            | Self::Renamed { time, .. }
            | Self::MetadataChanged { time, .. }
            | Self::ContentChanged { time, .. }
            | Self::Backend(BackendChange::LustreHsm { time, .. })
            | Self::Backend(BackendChange::LustreLayout { time, .. }) => *time,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Created {
                kind: EntryKind::Directory,
                ..
            } => "MKDIR",
            Self::Created {
                kind: EntryKind::Symlink,
                ..
            } => "SLINK",
            Self::Created { .. } => "CREAT",
            Self::Hardlinked { .. } => "HLINK",
            Self::Removed { directory: true, .. } => "RMDIR",
            Self::Removed { .. } => "UNLNK",
            Self::Renamed { .. } => "RENME",
            Self::MetadataChanged {
                kind: MetadataChangeKind::Xattr,
                ..
            } => "XATTR",
            Self::MetadataChanged { .. } => "SATTR",
            Self::ContentChanged {
                kind: ContentChangeKind::Truncate,
                ..
            } => "TRUNC",
            Self::ContentChanged { .. } => "CLOSE",
            Self::Backend(BackendChange::LustreHsm { .. }) => "HSM",
            Self::Backend(BackendChange::LustreLayout { .. }) => "LYOUT",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendChange {
    LustreHsm {
        object: ObjectId,
        event: u8,
        flags: u8,
        error: u8,
        time: i64,
    },
    LustreLayout {
        object: ObjectId,
        time: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ChangeSourceError {
    #[error("change stream closed")]
    Closed,
    #[error("checkpoint channel closed")]
    CommitChannelClosed,
    #[error("the previous batch has not been committed")]
    PendingCheckpoint,
    #[error("checkpoint belongs to {actual}, expected {expected}")]
    WrongSource { expected: String, actual: String },
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("gRPC request failed: {0}")]
    Rpc(#[from] tonic::Status),
    #[error("JuiceFS changelog retention gap: {0}")]
    RetentionGap(String),
    #[error("malformed JuiceFS changelog record: {0}")]
    MalformedRecord(String),
    #[error("record belongs to volume {actual}, expected {expected}")]
    WrongVolume { expected: String, actual: String },
    #[error("invalid changelog version {actual}; last acknowledged was {last}")]
    OutOfOrder { last: u64, actual: i64 },
}

#[async_trait]
pub trait ChangeSource: Send {
    async fn next_batch(&mut self) -> Result<Option<ChangeBatch>, ChangeSourceError>;
    async fn commit(&mut self, checkpoint: Checkpoint) -> Result<(), ChangeSourceError>;
}

/// Lustre adapter. `ListenerHandle` remains responsible for liblustreapi,
/// cursor persistence, and safe server-side changelog clearing.
pub struct LustreChangeSource {
    filesystem: FileSystemId,
    handle: ListenerHandle,
    pending: Option<Checkpoint>,
    ready: VecDeque<ChangeBatch>,
}

impl LustreChangeSource {
    pub fn new(filesystem: FileSystemId, handle: ListenerHandle) -> Self {
        Self {
            filesystem,
            handle,
            pending: None,
            ready: VecDeque::new(),
        }
    }
}

#[async_trait]
impl ChangeSource for LustreChangeSource {
    #[tracing::instrument(skip(self), fields(filesystem = %self.filesystem))]
    async fn next_batch(&mut self) -> Result<Option<ChangeBatch>, ChangeSourceError> {
        if self.pending.is_some() {
            return Err(ChangeSourceError::PendingCheckpoint);
        }
        if let Some(batch) = self.ready.pop_front() {
            self.pending = Some(batch.checkpoint.clone());
            return Ok(Some(batch));
        }
        let Some(batch) = self.handle.events.recv().await else {
            return Ok(None);
        };
        if batch.events.is_empty() {
            self.ready.push_back(ChangeBatch {
                filesystem: self.filesystem.clone(),
                changes: Vec::new(),
                checkpoint: Checkpoint {
                    source: batch.mdt,
                    position: batch.max_index,
                },
            });
        } else {
            self.ready.extend(batch.events.into_iter().map(|envelope| ChangeBatch {
                filesystem: self.filesystem.clone(),
                changes: vec![normalize_lustre(envelope.event, envelope.time)],
                checkpoint: Checkpoint {
                    source: envelope.mdt,
                    position: envelope.index,
                },
            }));
        }
        let next = self.ready.pop_front().expect("one batch was queued");
        self.pending = Some(next.checkpoint.clone());
        Ok(Some(next))
    }

    #[tracing::instrument(skip(self), fields(filesystem = %self.filesystem, source = %checkpoint.source, position = checkpoint.position))]
    async fn commit(&mut self, checkpoint: Checkpoint) -> Result<(), ChangeSourceError> {
        let expected = self.pending.as_ref().ok_or(ChangeSourceError::Closed)?;
        if expected != &checkpoint {
            return Err(ChangeSourceError::WrongSource {
                expected: format!("{}@{}", expected.source, expected.position),
                actual: format!("{}@{}", checkpoint.source, checkpoint.position),
            });
        }
        self.handle
            .acks
            .send(EventAck {
                mdt: checkpoint.source,
                committed_index: checkpoint.position,
            })
            .await
            .map_err(|_| ChangeSourceError::CommitChannelClosed)?;
        self.pending = None;
        Ok(())
    }
}

fn normalize_lustre(event: ChangelogEvent, time: i64) -> Change {
    let lustre = ObjectId::Lustre;
    match event {
        ChangelogEvent::Create { fid, parent, name, .. } => Change::Created {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            kind: EntryKind::File,
            metadata: None,
            time,
        },
        ChangelogEvent::Mkdir { fid, parent, name, .. } => Change::Created {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            kind: EntryKind::Directory,
            metadata: None,
            time,
        },
        ChangelogEvent::Mknod { fid, parent, name, .. } => Change::Created {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            kind: EntryKind::File,
            metadata: None,
            time,
        },
        ChangelogEvent::Softlink { fid, parent, name } => Change::Created {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            kind: EntryKind::Symlink,
            metadata: None,
            time,
        },
        ChangelogEvent::Hardlink { fid, parent, name } => Change::Hardlinked {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            time,
        },
        ChangelogEvent::Unlink {
            fid,
            parent,
            name,
            last_link,
            ..
        } => Change::Removed {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            last_link,
            directory: false,
            time,
        },
        ChangelogEvent::Rmdir { fid, parent, name } => Change::Removed {
            object: lustre(fid),
            parent: lustre(parent),
            name,
            last_link: true,
            directory: true,
            time,
        },
        ChangelogEvent::Rename {
            fid,
            parent,
            name,
            src_parent,
            src_name,
        } => Change::Renamed {
            object: lustre(fid),
            source_parent: lustre(src_parent),
            source_name: src_name,
            parent: lustre(parent),
            name,
            time,
        },
        ChangelogEvent::Close { fid, parent, name } => Change::ContentChanged {
            object: lustre(fid),
            parent: (!parent.is_zero()).then_some(lustre(parent)),
            name,
            kind: ContentChangeKind::Data,
            time,
        },
        ChangelogEvent::Truncate { fid } => Change::ContentChanged {
            object: lustre(fid),
            parent: None,
            name: Bytes::new(),
            kind: ContentChangeKind::Truncate,
            time,
        },
        ChangelogEvent::SetAttr { fid } | ChangelogEvent::MTime { fid } | ChangelogEvent::CTime { fid } => {
            Change::MetadataChanged {
                object: lustre(fid),
                kind: MetadataChangeKind::Attributes,
                time,
            }
        }
        ChangelogEvent::XAttr { fid, .. } => Change::MetadataChanged {
            object: lustre(fid),
            kind: MetadataChangeKind::Xattr,
            time,
        },
        ChangelogEvent::Hsm {
            fid,
            hsm_event,
            hsm_flags,
            hsm_error,
        } => BackendChange::LustreHsm {
            object: lustre(fid),
            event: hsm_event,
            flags: hsm_flags,
            error: hsm_error,
            time,
        }
        .into(),
        ChangelogEvent::Layout { fid } => BackendChange::LustreLayout {
            object: lustre(fid),
            time,
        }
        .into(),
    }
}

impl From<BackendChange> for Change {
    fn from(value: BackendChange) -> Self {
        Self::Backend(value)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use lustre_api::LuFid;
    use lustre_changelog::{ChangelogEvent, ChangelogEventEnvelope, EventAck, EventBatch, ListenerHandle};
    use rbh_entry_store::model::{EntryKind, FileSystemId, ObjectId};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::{Change, ChangeSource, LustreChangeSource};

    #[tokio::test]
    async fn lustre_batch_is_filesystem_scoped_and_checkpointed_after_commit() {
        let (event_tx, event_rx) = mpsc::channel(1);
        let (ack_tx, mut ack_rx) = mpsc::channel(1);
        let fid = LuFid::new(0x200000401, 2, 0);
        let parent = LuFid::new(0x200000401, 1, 0);
        event_tx
            .send(EventBatch {
                mdt: "testfs-MDT0000".into(),
                events: vec![ChangelogEventEnvelope::new(
                    "testfs-MDT0000",
                    41,
                    1_700_000_000,
                    ChangelogEvent::Create {
                        fid,
                        parent,
                        name: Bytes::from_static(b"report"),
                        jobid: None,
                        uid: None,
                        gid: None,
                    },
                )],
                min_index: 41,
                max_index: 41,
            })
            .await
            .unwrap();
        drop(event_tx);

        let handle = ListenerHandle {
            events: event_rx,
            acks: ack_tx,
            cancel: CancellationToken::new(),
        };
        let filesystem = FileSystemId::new("archive").unwrap();
        let mut source = LustreChangeSource::new(filesystem.clone(), handle);

        let batch = source.next_batch().await.unwrap().unwrap();
        assert_eq!(batch.filesystem, filesystem);
        assert_eq!(batch.checkpoint.position, 41);
        assert_eq!(batch.changes.len(), 1);
        assert!(matches!(
            &batch.changes[0],
            Change::Created {
                object: ObjectId::Lustre(value),
                parent: ObjectId::Lustre(parent_value),
                name,
                kind: EntryKind::File,
                ..
            } if *value == fid && *parent_value == parent && name.as_ref() == b"report"
        ));
        assert!(
            ack_rx.try_recv().is_err(),
            "reading a batch must not advance its checkpoint"
        );

        source.commit(batch.checkpoint).await.unwrap();
        let EventAck { mdt, committed_index } = ack_rx.recv().await.unwrap();
        assert_eq!(mdt, "testfs-MDT0000");
        assert_eq!(committed_index, 41);
    }

    #[tokio::test]
    async fn lustre_source_rejects_checkpoint_that_was_not_delivered() {
        let (event_tx, event_rx) = mpsc::channel(1);
        let (ack_tx, mut ack_rx) = mpsc::channel(1);
        event_tx
            .send(EventBatch {
                mdt: "testfs-MDT0000".into(),
                events: vec![],
                min_index: 90,
                max_index: 90,
            })
            .await
            .unwrap();
        let handle = ListenerHandle {
            events: event_rx,
            acks: ack_tx,
            cancel: CancellationToken::new(),
        };
        let mut source = LustreChangeSource::new(FileSystemId::new("archive").unwrap(), handle);
        source.next_batch().await.unwrap().unwrap();

        let error = source
            .commit(crate::Checkpoint {
                source: "testfs-MDT0000".into(),
                position: 91,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, crate::ChangeSourceError::WrongSource { .. }));
        assert!(ack_rx.try_recv().is_err());
    }
}
