use std::time::{SystemTime, UNIX_EPOCH};

use rbh_entry_store::{BackendKind, EntryKey, EntryKind, EntryStore, FileSystemId, ObjectId, ScopedEntryRow};
use rbh_namespace::{NamespaceAdapter, NamespaceError, NamespaceTarget};

use crate::ActionError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendActionOutcome {
    Success,
    AlreadyMissing,
    Failed { error: String, retryable: bool },
}

#[async_trait::async_trait]
pub trait BackendAction: Send + Sync {
    async fn purge(&self, entry: &ScopedEntryRow) -> Result<BackendActionOutcome, ActionError>;
    async fn record_retryable_failure(&self, _entry: &ScopedEntryRow, _error: &str) -> Result<(), ActionError> {
        Ok(())
    }
}

/// Filesystem-bound action module. Backend selection, namespace safety,
/// capability checks and catalog result persistence stay behind this seam.
#[derive(Clone)]
pub struct ActionBackend {
    store: EntryStore,
    filesystem: FileSystemId,
    backend: BackendKind,
    namespace: NamespaceAdapter,
}

impl ActionBackend {
    pub async fn new(store: EntryStore, filesystem: FileSystemId) -> Result<Self, ActionError> {
        let config = store
            .get_filesystem(&filesystem)
            .await
            .map_err(|error| ActionError::Store(error.to_string()))?
            .ok_or_else(|| ActionError::Capability(format!("unknown filesystem: {filesystem}")))?;
        if !config.capabilities.purge {
            return Err(ActionError::Capability(format!(
                "filesystem {filesystem} does not provide purge capability"
            )));
        }
        let namespace = NamespaceAdapter::new(store.clone(), filesystem.clone()).await?;
        Ok(Self {
            store,
            filesystem,
            backend: config.backend,
            namespace,
        })
    }

    async fn execute_purge(&self, entry: &ScopedEntryRow) -> Result<BackendActionOutcome, ActionError> {
        if entry.key.filesystem() != &self.filesystem || entry.key.object().backend() != self.backend {
            return Err(ActionError::Capability(format!(
                "entry {:?} does not belong to backend {}",
                entry.key, self.filesystem
            )));
        }
        let resolved = match self.namespace.resolve(NamespaceTarget::Object(entry.key.clone())).await {
            Ok(resolved) => resolved,
            Err(NamespaceError::NotFound(_) | NamespaceError::StalePath(_)) => {
                self.persist_missing(entry).await?;
                return Ok(BackendActionOutcome::AlreadyMissing);
            }
            Err(error) => {
                let message = error.to_string();
                let retryable = namespace_retryable(&error);
                self.persist_failure(&entry.key, &message, retryable).await?;
                return Ok(BackendActionOutcome::Failed {
                    error: message,
                    retryable,
                });
            }
        };
        let parent = match resolved.path.parent() {
            Some(path) => match self.namespace.resolve(NamespaceTarget::Path(path.to_path_buf())).await {
                Ok(parent) => parent.key,
                Err(error) => {
                    let retryable = namespace_retryable(&error);
                    let message = error.to_string();
                    self.persist_failure(&entry.key, &message, retryable).await?;
                    return Ok(BackendActionOutcome::Failed {
                        error: message,
                        retryable,
                    });
                }
            },
            None => return Err(ActionError::NoPath),
        };
        let name = resolved
            .path
            .file_name()
            .ok_or(ActionError::NoPath)?
            .as_encoded_bytes()
            .to_vec();
        let operation = match resolved.stat.kind {
            EntryKind::Directory | EntryKind::File | EntryKind::Symlink => {
                self.capture_verify_and_remove(&resolved.path, &entry.key, resolved.stat.kind)
                    .await
            }
            kind => {
                let error = format!("purge does not support {kind:?}");
                self.persist_failure(&entry.key, &error, false).await?;
                return Ok(BackendActionOutcome::Failed {
                    error,
                    retryable: false,
                });
            }
        };
        match operation {
            Ok(()) => {
                self.persist_unlink(entry, *parent.object(), &name, "success").await?;
                Ok(BackendActionOutcome::Success)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let message = "namespace target disappeared during atomic purge capture".to_string();
                self.persist_failure(&entry.key, &message, true).await?;
                Ok(BackendActionOutcome::Failed {
                    error: message,
                    retryable: true,
                })
            }
            Err(error) => {
                let retryable = retryable_io(error.kind());
                let message = error.to_string();
                self.persist_failure(&entry.key, &message, retryable).await?;
                Ok(BackendActionOutcome::Failed {
                    error: message,
                    retryable,
                })
            }
        }
    }

    async fn capture_verify_and_remove(
        &self, path: &std::path::Path, key: &EntryKey, kind: EntryKind,
    ) -> Result<(), std::io::Error> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("purge target has no parent"))?;
        let quarantine = parent.join(format!(".rbh-purge-{}", uuid::Uuid::new_v4().simple()));
        let source = path.to_path_buf();
        let captured = quarantine.clone();
        let backend = self.backend;
        tokio::task::spawn_blocking(move || capture_name(&source, &captured, backend))
            .await
            .map_err(std::io::Error::other)??;

        let matches = match (self.backend, *key.object()) {
            (BackendKind::JuiceFs, ObjectId::JuiceFs(expected)) => std::fs::symlink_metadata(&quarantine)
                .map(|metadata| std::os::unix::fs::MetadataExt::ino(&metadata) == expected),
            (BackendKind::Lustre, ObjectId::Lustre(expected)) => lustre_api::LustreApi
                .path_to_fid(&quarantine)
                .map(|actual| actual == expected)
                .map_err(std::io::Error::other),
            _ => Ok(false),
        }?;
        if !matches {
            let restore_from = quarantine.clone();
            let restore_to = path.to_path_buf();
            let restore = tokio::task::spawn_blocking(move || rename_noreplace(&restore_from, &restore_to))
                .await
                .map_err(std::io::Error::other)?;
            return match restore {
                Ok(()) => Err(race_error("captured namespace object did not match catalog identity")),
                Err(error) => Err(race_error(format!(
                    "captured replacement preserved at {} because restore raced: {error}",
                    quarantine.display()
                ))),
            };
        }
        let removal = match kind {
            EntryKind::Directory => tokio::fs::remove_dir(&quarantine).await,
            _ => tokio::fs::remove_file(&quarantine).await,
        };
        if let Err(error) = removal {
            let restore_from = quarantine.clone();
            let restore_to = path.to_path_buf();
            let restore = tokio::task::spawn_blocking(move || rename_noreplace(&restore_from, &restore_to))
                .await
                .map_err(std::io::Error::other)?;
            return match restore {
                Ok(()) => Err(error),
                Err(restore_error) => Err(race_error(format!(
                    "failed target preserved at {} because restore raced: {restore_error}; removal error: {error}",
                    quarantine.display()
                ))),
            };
        }
        Ok(())
    }

    async fn persist_missing(&self, entry: &ScopedEntryRow) -> Result<(), ActionError> {
        let Some(parent) = entry.parent.as_ref() else {
            return Ok(());
        };
        self.persist_unlink(entry, *parent.object(), &entry.name, "already_missing")
            .await
    }

    async fn persist_unlink(
        &self, entry: &ScopedEntryRow, parent: ObjectId, name: &[u8], state: &'static str,
    ) -> Result<(), ActionError> {
        self.store
            .patch_scoped_sm_status(
                &entry.key,
                &serde_json::json!({"action": {"kind": "purge", "state": state, "updated_at": now()}}),
            )
            .await
            .map_err(|error| ActionError::Store(error.to_string()))?;
        self.store
            .apply_scoped_unlink(&entry.key, parent, name, now(), entry.kind == EntryKind::Directory)
            .await
            .map_err(|error| ActionError::Store(error.to_string()))
    }

    async fn persist_failure(&self, key: &EntryKey, error: &str, retryable: bool) -> Result<(), ActionError> {
        self.store
            .patch_scoped_sm_status(
                key,
                &serde_json::json!({"action": {"kind": "purge", "state": "failed", "error": error, "retryable": retryable, "updated_at": now()}}),
            )
            .await
            .map_err(|source| ActionError::Store(source.to_string()))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl BackendAction for ActionBackend {
    async fn purge(&self, entry: &ScopedEntryRow) -> Result<BackendActionOutcome, ActionError> {
        self.execute_purge(entry).await
    }

    async fn record_retryable_failure(&self, entry: &ScopedEntryRow, error: &str) -> Result<(), ActionError> {
        self.persist_failure(&entry.key, error, true).await
    }
}

fn retryable_io(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::OutOfMemory
    )
}

fn namespace_retryable(error: &NamespaceError) -> bool {
    match error {
        NamespaceError::Io { source, .. } => retryable_io(source.kind()),
        NamespaceError::Store(_) | NamespaceError::Join(_) | NamespaceError::MissingParent(_) => true,
        NamespaceError::Lustre(_) => true,
        NamespaceError::WrongFilesystem { .. }
        | NamespaceError::OutsideFilesystem { .. }
        | NamespaceError::NotFound(_)
        | NamespaceError::StalePath(_)
        | NamespaceError::Cycle(_)
        | NamespaceError::BackendMismatch(_) => false,
    }
}

fn rename_noreplace(source: &std::path::Path, destination: &std::path::Path) -> Result<(), std::io::Error> {
    nix::fcntl::renameat2(
        None,
        source,
        None,
        destination,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))
}

fn capture_name(
    source: &std::path::Path, destination: &std::path::Path, backend: BackendKind,
) -> Result<(), std::io::Error> {
    match rename_noreplace(source, destination) {
        Err(error) if error.raw_os_error() == Some(nix::libc::EINVAL) && backend == BackendKind::Lustre => {
            std::fs::rename(source, destination)
        }
        result => result,
    }
}

fn race_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::WouldBlock, message.into())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::{namespace_retryable, race_error, retryable_io};

    #[test]
    fn permission_failures_are_terminal_but_transient_io_is_retryable() {
        assert!(!retryable_io(std::io::ErrorKind::PermissionDenied));
        assert!(retryable_io(std::io::ErrorKind::Interrupted));
        assert!(retryable_io(std::io::ErrorKind::WouldBlock));
        assert!(retryable_io(std::io::ErrorKind::TimedOut));
        let permission = rbh_namespace::NamespaceError::Io {
            path: "/denied".into(),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        assert!(!namespace_retryable(&permission));
        assert!(retryable_io(race_error("replacement race").kind()));
    }
}
