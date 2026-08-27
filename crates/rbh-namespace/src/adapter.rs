use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use rbh_entry_store::store::EntryStore;
use rbh_entry_store::{BackendKind, EntryKey, EntryKind, FileSystemConfig, FileSystemId, ObjectId};

use crate::{NamespaceError, NamespaceStat, NamespaceTarget, ResolvedNamespace};

/// Runtime-selected native namespace adapter. Construction binds the adapter
/// permanently to one registered filesystem.
#[derive(Clone)]
pub struct NamespaceAdapter {
    store: EntryStore,
    config: FileSystemConfig,
}

impl NamespaceAdapter {
    #[tracing::instrument(name = "namespace.new", skip(store), fields(filesystem = %filesystem))]
    pub async fn new(store: EntryStore, filesystem: FileSystemId) -> Result<Self, NamespaceError> {
        let config = store
            .get_filesystem(&filesystem)
            .await?
            .ok_or_else(|| NamespaceError::BackendMismatch(filesystem.clone()))?;
        if !config.capabilities.namespace {
            return Err(NamespaceError::BackendMismatch(filesystem));
        }
        Ok(Self { store, config })
    }

    fn check_key(&self, key: &EntryKey) -> Result<(), NamespaceError> {
        if key.filesystem() != &self.config.id {
            return Err(NamespaceError::WrongFilesystem {
                expected: self.config.id.clone(),
                actual: key.filesystem().clone(),
            });
        }
        Ok(())
    }

    async fn relative_path(&self, path: &Path) -> Result<PathBuf, NamespaceError> {
        if path.components().any(|part| matches!(part, Component::ParentDir)) {
            return Err(NamespaceError::OutsideFilesystem {
                filesystem: self.config.id.clone(),
                path: path.to_path_buf(),
            });
        }
        let relative = path
            .strip_prefix(&self.config.mount_path)
            .map_err(|_| NamespaceError::OutsideFilesystem {
                filesystem: self.config.id.clone(),
                path: path.to_path_buf(),
            })?;
        // Resolve the parent, not the final component: this rejects traversal
        // through a symlink that escapes the configured mount while still
        // allowing the final object itself to be a symlink.
        if relative.as_os_str().is_empty() {
            return Ok(PathBuf::new());
        }
        let mount_path = self.config.mount_path.clone();
        let parent_path = path.parent().unwrap_or(path).to_path_buf();
        let (mount, parent) = tokio::task::spawn_blocking(move || {
            let mount = std::fs::canonicalize(&mount_path).map_err(|source| NamespaceError::Io {
                path: mount_path,
                source,
            })?;
            let parent = std::fs::canonicalize(&parent_path).map_err(|source| NamespaceError::Io {
                path: parent_path,
                source,
            })?;
            Ok::<_, NamespaceError>((mount, parent))
        })
        .await??;
        if !parent.starts_with(mount) {
            return Err(NamespaceError::OutsideFilesystem {
                filesystem: self.config.id.clone(),
                path: path.to_path_buf(),
            });
        }
        Ok(relative.to_path_buf())
    }

    async fn stat(path: PathBuf) -> Result<NamespaceStat, NamespaceError> {
        let task_path = path.clone();
        let metadata = tokio::task::spawn_blocking(move || std::fs::symlink_metadata(&task_path))
            .await?
            .map_err(|source| NamespaceError::Io { path, source })?;
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
        Ok(NamespaceStat {
            kind,
            size: metadata.len(),
            nlink: metadata.nlink(),
            inode: metadata.ino(),
        })
    }

    async fn juice_root(&self) -> Result<EntryKey, NamespaceError> {
        let stat = Self::stat(self.config.mount_path.clone()).await?;
        let key = EntryKey::new(self.config.id.clone(), ObjectId::JuiceFs(stat.inode));
        self.store
            .get_scoped_entry(&key)
            .await?
            .ok_or_else(|| NamespaceError::NotFound(key.clone()))?;
        Ok(key)
    }

    async fn juice_parent_path(&self, key: &EntryKey) -> Result<PathBuf, NamespaceError> {
        let root = self.juice_root().await?;
        let mut current = key.clone();
        let mut names = Vec::new();
        let mut visited = HashSet::new();
        while current != root {
            if !visited.insert(current.clone()) {
                return Err(NamespaceError::Cycle(current));
            }
            let entry = self
                .store
                .get_scoped_entry(&current)
                .await?
                .ok_or_else(|| NamespaceError::MissingParent(current.clone()))?;
            names.push(entry.name);
            current = entry
                .parent
                .ok_or_else(|| NamespaceError::MissingParent(current.clone()))?;
        }
        let mut path = self.config.mount_path.clone();
        for name in names.iter().rev() {
            path.push(OsStr::from_bytes(name));
        }
        Ok(path)
    }

    async fn resolve_juice_object(&self, key: EntryKey) -> Result<ResolvedNamespace, NamespaceError> {
        self.check_key(&key)?;
        let ObjectId::JuiceFs(expected_inode) = key.object() else {
            return Err(NamespaceError::BackendMismatch(self.config.id.clone()));
        };
        let root = self.juice_root().await?;
        if key == root {
            let stat = Self::stat(self.config.mount_path.clone()).await?;
            return Ok(ResolvedNamespace {
                key,
                path: self.config.mount_path.clone(),
                stat,
            });
        }
        if self.store.get_scoped_entry(&key).await?.is_none() {
            return Err(NamespaceError::NotFound(key));
        }
        let edges = self.store.list_scoped_object_edges(&key).await?;
        if edges.is_empty() {
            return Err(NamespaceError::MissingParent(key));
        }
        let mut resolved_parent = false;
        for edge in edges {
            let parent = EntryKey::new(self.config.id.clone(), edge.parent);
            let mut path = match self.juice_parent_path(&parent).await {
                Ok(path) => path,
                Err(NamespaceError::MissingParent(_) | NamespaceError::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            resolved_parent = true;
            path.push(OsStr::from_bytes(&edge.name));
            self.relative_path(&path).await?;
            if let Ok(stat) = Self::stat(path.clone()).await
                && stat.inode == *expected_inode
            {
                return Ok(ResolvedNamespace { key, path, stat });
            }
        }
        if resolved_parent {
            Err(NamespaceError::StalePath(key))
        } else {
            Err(NamespaceError::MissingParent(key))
        }
    }

    async fn resolve_juice_path(&self, path: PathBuf) -> Result<ResolvedNamespace, NamespaceError> {
        let relative = self.relative_path(&path).await?;
        let stat = Self::stat(path.clone()).await?;
        let mut key = self.juice_root().await?;
        for component in relative.components() {
            let Component::Normal(name) = component else { continue };
            key = self
                .store
                .lookup_scoped_namespace_child(&self.config.id, *key.object(), name.as_bytes())
                .await?
                .ok_or_else(|| NamespaceError::StalePath(key.clone()))?;
        }
        if key.object() != &ObjectId::JuiceFs(stat.inode) {
            return Err(NamespaceError::StalePath(key));
        }
        Ok(ResolvedNamespace { key, path, stat })
    }

    async fn resolve_lustre(&self, target: NamespaceTarget) -> Result<ResolvedNamespace, NamespaceError> {
        let lustre = lustre_api::LustreApi;
        let (key, path) = match target {
            NamespaceTarget::Path(path) => {
                self.relative_path(&path).await?;
                let ffi_path = path.clone();
                let fid = tokio::task::spawn_blocking(move || lustre.path_to_fid(&ffi_path)).await??;
                (EntryKey::new(self.config.id.clone(), ObjectId::Lustre(fid)), path)
            }
            NamespaceTarget::Object(key) => {
                self.check_key(&key)?;
                let ObjectId::Lustre(fid) = *key.object() else {
                    return Err(NamespaceError::BackendMismatch(self.config.id.clone()));
                };
                let mount = self.config.mount_path.clone();
                let device = mount.to_string_lossy().into_owned();
                let relative = tokio::task::spawn_blocking(move || lustre.fid_to_path(&device, &fid)).await?;
                let relative = relative.map_err(|_| NamespaceError::StalePath(key.clone()))?;
                if relative.is_absolute() {
                    return Err(NamespaceError::StalePath(key));
                }
                let path = mount.join(relative);
                self.relative_path(&path).await?;
                let verify_path = path.clone();
                let actual = tokio::task::spawn_blocking(move || lustre.path_to_fid(&verify_path)).await?;
                if actual.ok() != Some(fid) {
                    return Err(NamespaceError::StalePath(key));
                }
                (key, path)
            }
        };
        let stat = Self::stat(path.clone()).await.map_err(|error| match error {
            NamespaceError::Io { .. } => NamespaceError::StalePath(key.clone()),
            other => other,
        })?;
        Ok(ResolvedNamespace { key, path, stat })
    }
}

impl NamespaceAdapter {
    pub fn filesystem(&self) -> &FileSystemId {
        &self.config.id
    }

    /// Resolve a path or native object id and validate its live metadata.
    /// Backend details, graph traversal, mount containment, stale-edge
    /// handling, and stat validation remain behind this single interface.
    #[tracing::instrument(name = "namespace.resolve", skip(self), fields(filesystem = %self.config.id))]
    pub async fn resolve(&self, target: NamespaceTarget) -> Result<ResolvedNamespace, NamespaceError> {
        match self.config.backend {
            BackendKind::Lustre => self.resolve_lustre(target).await,
            BackendKind::JuiceFs => match target {
                NamespaceTarget::Object(key) => self.resolve_juice_object(key).await,
                NamespaceTarget::Path(path) => self.resolve_juice_path(path).await,
            },
        }
    }
}
