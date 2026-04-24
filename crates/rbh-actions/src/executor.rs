//! Action executor trait and concrete implementations.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lustre_api::LustreApi;
use lustre_api::hsm::{HsmAction, HsmRequestBuilder};
use rbh_entry_store::model::{EntryKind, EntryRow};

use crate::ActionError;

/// Outcome of executing an action on a single entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Action completed successfully.
    Success,
    /// Entry was skipped (e.g., already in desired state).
    Skipped { reason: String },
    /// Action failed for this entry.
    Failed { error: String },
}

/// Shared context for action execution.
pub struct ActionContext {
    /// Lustre mount point (e.g., `/lustre`).
    pub mount_path: PathBuf,
    /// Lustre API handle for HSM operations.
    pub lustre: LustreApi,
}

/// Trait for executing a policy action on a single entry.
#[async_trait]
pub trait ActionExecutor: Send + Sync {
    /// Execute the action on the given entry.
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError>;
}

/// Resolve a FID to its real filesystem path via `llapi_fid2path`.
///
/// `llapi_fid2path` returns a path relative to the mount root. This function
/// prepends the mount path to produce an absolute path suitable for all
/// filesystem operations (stat, open, unlink, rename).
///
/// Returns `None` if the FID no longer exists on the filesystem.
fn resolve_real_path(lustre: &LustreApi, mount: &Path, fid: &lustre_api::LuFid) -> Option<PathBuf> {
    let mount_str = mount.to_string_lossy();
    match lustre.fid_to_path(&mount_str, fid) {
        Ok(rel_path) => {
            // llapi_fid2path returns relative path; make it absolute.
            let abs = mount.join(rel_path);
            Some(abs)
        }
        Err(e) => {
            tracing::warn!(fid = %fid, error = %e, "fid_to_path failed");
            None
        }
    }
}

/// Build the `.lustre/fid/<FID>` virtual path. Only works for open/stat/ioctl,
/// NOT for unlink/rename.
fn fid_path(mount: &Path, entry: &EntryRow) -> PathBuf {
    // Strip brackets from FID Display format: [0xseq:0xoid:0xver] → 0xseq:0xoid:0xver
    let fid_str = entry.fid.to_string();
    let stripped = fid_str.trim_start_matches('[').trim_end_matches(']');
    mount.join(".lustre").join("fid").join(stripped)
}

// ---------------------------------------------------------------------------
// Purge (unlink / rmdir)
// ---------------------------------------------------------------------------

/// Removes files (`unlink`) or empty directories (`rmdir`).
pub struct PurgeExecutor;

#[async_trait]
impl ActionExecutor for PurgeExecutor {
    #[tracing::instrument(name = "action.purge", skip(self, ctx), fields(fid = %entry.fid))]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        // Purge requires the real path (unlink doesn't work via .lustre/fid/).
        let lustre = ctx.lustre;
        let mount = ctx.mount_path.clone();
        let fid = entry.fid;
        let path = tokio::task::spawn_blocking(move || resolve_real_path(&lustre, &mount, &fid))
            .await
            .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))?;
        let path = match path {
            Some(p) => p,
            None => {
                return Ok(ActionOutcome::Skipped {
                    reason: "could not resolve FID to path".to_string(),
                });
            }
        };

        match entry.kind {
            EntryKind::File => match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    tracing::info!("purged file");
                    Ok(ActionOutcome::Success)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ActionOutcome::Skipped {
                    reason: "file already removed".to_string(),
                }),
                Err(e) => Err(ActionError::Io(e)),
            },
            EntryKind::Directory => match tokio::fs::remove_dir(&path).await {
                Ok(()) => {
                    tracing::info!("purged directory");
                    Ok(ActionOutcome::Success)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ActionOutcome::Skipped {
                    reason: "directory already removed".to_string(),
                }),
                Err(e) => Err(ActionError::Io(e)),
            },
            _ => Ok(ActionOutcome::Skipped {
                reason: format!("unsupported entry kind: {:?}", entry.kind),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// HSM Archive
// ---------------------------------------------------------------------------

/// Submits an HSM archive request for the entry's FID.
pub struct HsmArchiveExecutor {
    /// HSM archive backend id (typically 1).
    pub archive_id: u32,
    /// Agent-specific hints passed as the HSM request payload. `None`
    /// sends an empty data block; the Lustre HSM coordinator ignores
    /// it but agents (copytool, lhsmtool_cmd) may interpret it.
    pub hints: Option<Vec<u8>>,
}

#[async_trait]
impl ActionExecutor for HsmArchiveExecutor {
    #[tracing::instrument(
        name = "action.hsm_archive",
        skip(self, ctx),
        fields(fid = %entry.fid, archive_id = self.archive_id)
    )]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        if entry.kind != EntryKind::File {
            return Ok(ActionOutcome::Skipped {
                reason: "HSM archive only applies to files".to_string(),
            });
        }

        // Check current HSM state — skip if already archived and not dirty.
        let path = fid_path(&ctx.mount_path, entry);
        let state = {
            let p = path.clone();
            let lustre = ctx.lustre;
            tokio::task::spawn_blocking(move || lustre.hsm_state_get(&p))
                .await
                .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??
        };

        if state.states.contains(lustre_api::hsm::HsmState::ARCHIVED)
            && !state.states.contains(lustre_api::hsm::HsmState::DIRTY)
        {
            return Ok(ActionOutcome::Skipped {
                reason: "already archived and clean".to_string(),
            });
        }

        // Submit archive request via spawn_blocking (FFI is sync).
        let fid = entry.fid;
        let archive_id = self.archive_id;
        let hints = self.hints.clone();
        let mount = ctx.mount_path.clone();
        let lustre = ctx.lustre;
        tokio::task::spawn_blocking(move || {
            let mut builder = HsmRequestBuilder::new(HsmAction::Archive)
                .archive_id(archive_id)
                .add_fid(fid);
            if let Some(h) = hints {
                builder = builder.data(h);
            }
            builder.submit(&lustre, &mount)
        })
        .await
        .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??;

        tracing::info!(archive_id, "HSM archive request submitted");
        Ok(ActionOutcome::Success)
    }
}

// ---------------------------------------------------------------------------
// HSM Release
// ---------------------------------------------------------------------------

/// Submits an HSM release request to free OST space for archived files.
pub struct HsmReleaseExecutor;

#[async_trait]
impl ActionExecutor for HsmReleaseExecutor {
    #[tracing::instrument(
        name = "action.hsm_release",
        skip(self, ctx),
        fields(fid = %entry.fid)
    )]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        if entry.kind != EntryKind::File {
            return Ok(ActionOutcome::Skipped {
                reason: "HSM release only applies to files".to_string(),
            });
        }

        // Check HSM state — must be archived and not already released.
        let path = fid_path(&ctx.mount_path, entry);
        let state = {
            let p = path.clone();
            let lustre = ctx.lustre;
            tokio::task::spawn_blocking(move || lustre.hsm_state_get(&p))
                .await
                .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??
        };

        if !state.states.contains(lustre_api::hsm::HsmState::ARCHIVED) {
            return Ok(ActionOutcome::Skipped {
                reason: "file is not archived — cannot release".to_string(),
            });
        }
        if state.states.contains(lustre_api::hsm::HsmState::RELEASED) {
            return Ok(ActionOutcome::Skipped {
                reason: "already released".to_string(),
            });
        }

        let fid = entry.fid;
        let mount = ctx.mount_path.clone();
        let lustre = ctx.lustre;
        tokio::task::spawn_blocking(move || {
            HsmRequestBuilder::new(HsmAction::Release)
                .add_fid(fid)
                .submit(&lustre, &mount)
        })
        .await
        .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??;

        tracing::info!("HSM release request submitted");
        Ok(ActionOutcome::Success)
    }
}

// ---------------------------------------------------------------------------
// External backup (rbhext_tool)
// ---------------------------------------------------------------------------

/// Runs an external backup tool (matching robinhood-C's `rbhext_tool`
/// protocol) via a [`rbh_backup::BackupAdapter`].
///
/// Operation is selected by `op`:
/// * `Archive` — copy Lustre → backend
/// * `Restore` — copy backend → Lustre
/// * `Remove`  — delete the backend copy
///
/// The source path is always resolved from the entry's FID. The
/// destination path, when required, is either taken from the configured
/// `dest_template` (supports `{src}`, `{mount}`, `{archive_id}`) or
/// left empty so the tool itself can derive it (rbhext_tool scripts
/// typically compute dest from src).
pub struct BackupExecutor {
    pub adapter: std::sync::Arc<dyn rbh_backup::BackupAdapter>,
    pub op: rbh_backup::BackupOp,
    pub archive_id: u32,
    pub hints: Option<String>,
    /// Template rendered to produce the `dest` path. `None` → pass empty
    /// dest to the tool.
    pub dest_template: Option<String>,
}

impl BackupExecutor {
    /// Build from a parsed `BackupCommandConfig` with the given operation.
    pub fn from_config(
        cfg: &rbh_backup::BackupCommandConfig, op: rbh_backup::BackupOp, archive_id: u32, hints: Option<String>,
        dest_template: Option<String>,
    ) -> Self {
        Self {
            adapter: std::sync::Arc::new(cfg.build()),
            op,
            archive_id,
            hints,
            dest_template,
        }
    }

    fn render_dest(&self, src: &Path, mount: &Path) -> Option<PathBuf> {
        let tpl = self.dest_template.as_ref()?;
        let rendered = tpl
            .replace("{src}", &src.to_string_lossy())
            .replace("{mount}", &mount.to_string_lossy())
            .replace("{archive_id}", &self.archive_id.to_string());
        Some(PathBuf::from(rendered))
    }
}

#[async_trait]
impl ActionExecutor for BackupExecutor {
    #[tracing::instrument(
        name = "action.backup",
        skip(self, ctx),
        fields(fid = %entry.fid, op = ?self.op, archive_id = self.archive_id)
    )]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        // Resolve FID → real path (required by the rbhext_tool contract:
        // the script expects a regular filesystem path, not .lustre/fid/…).
        let lustre = ctx.lustre;
        let mount = ctx.mount_path.clone();
        let fid = entry.fid;
        let src = tokio::task::spawn_blocking(move || resolve_real_path(&lustre, &mount, &fid))
            .await
            .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))?;
        let src = match src {
            Some(p) => p,
            None => {
                return Ok(ActionOutcome::Skipped {
                    reason: "could not resolve FID to path".to_string(),
                });
            }
        };

        let dest = self.render_dest(&src, &ctx.mount_path);
        let inv = rbh_backup::ToolInvocation {
            op: self.op,
            src: &src,
            dest: dest.as_deref(),
            hints: self.hints.as_deref(),
            archive_id: self.archive_id,
        };

        let res = match self.op {
            rbh_backup::BackupOp::Archive => self.adapter.archive(&inv).await,
            rbh_backup::BackupOp::Restore => self.adapter.restore(&inv).await,
            rbh_backup::BackupOp::Remove => self.adapter.remove(&inv).await,
        };
        match res {
            Ok(()) => {
                tracing::info!(src = %src.display(), "backup tool succeeded");
                Ok(ActionOutcome::Success)
            }
            Err(rbh_backup::BackupError::ToolFailed { code, stderr }) => {
                tracing::warn!(code, %stderr, "backup tool failed");
                Ok(ActionOutcome::Failed {
                    error: format!("tool exit {code}: {stderr}"),
                })
            }
            Err(e) => Err(ActionError::Backup(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use lustre_api::LuFid;
    use rbh_entry_store::model::EntryKind;

    fn test_file_entry() -> EntryRow {
        EntryRow {
            fid: LuFid::new(0x200000401, 0x42, 0),
            parent_fid: Some(LuFid::new(0x200000401, 0x01, 0)),
            name: Bytes::from_static(b"test.dat"),
            kind: EntryKind::File,
            size: 1024,
            blocks: 8,
            uid: 1000,
            gid: 100,
            projid: 0,
            mode: 0o644,
            nlink: 1,
            atime: 1_775_955_820,
            mtime: 1_775_000_000,
            ctime: 1_775_000_000,
            stripe_count: None,
            stripe_size: None,
            pool_name: None,
            sm_status: serde_json::json!({}),
            last_seen: 1_775_955_820,
        }
    }

    #[test]
    fn fid_path_format() {
        let entry = test_file_entry();
        let path = fid_path(Path::new("/lustre"), &entry);
        // fid_path strips brackets from Display format
        let fid_str = entry.fid.to_string();
        let stripped = fid_str.trim_start_matches('[').trim_end_matches(']');
        let expected = format!("/lustre/.lustre/fid/{stripped}");
        assert_eq!(path.to_string_lossy(), expected);
    }

    #[test]
    fn action_outcome_variants() {
        let success = ActionOutcome::Success;
        let skipped = ActionOutcome::Skipped {
            reason: "test".to_string(),
        };
        let failed = ActionOutcome::Failed {
            error: "err".to_string(),
        };
        assert_eq!(success, ActionOutcome::Success);
        assert_ne!(skipped, failed);
    }

    #[tokio::test]
    async fn purge_nonexistent_file_skips() {
        let entry = test_file_entry();
        let ctx = ActionContext {
            mount_path: PathBuf::from("/nonexistent_mount_for_test"),
            lustre: LustreApi,
        };
        let exec = PurgeExecutor;
        let result = exec.execute(&entry, &ctx).await;
        // The .lustre/fid path won't exist, so we expect NotFound → Skipped
        match result {
            Ok(ActionOutcome::Skipped { .. }) => {}
            other => panic!("expected Skipped for nonexistent path, got: {other:?}"),
        }
    }
}
