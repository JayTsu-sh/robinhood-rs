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
// HSM Restore — drives PolicyKind::HsmRestore
// ---------------------------------------------------------------------------

/// Submits an HSM restore request to bring a released file back to Lustre.
///
/// Only acts on files that are currently in the `released` state; files
/// that are already present on OSTs (not released) are skipped.
pub struct HsmRestoreExecutor;

#[async_trait]
impl ActionExecutor for HsmRestoreExecutor {
    #[tracing::instrument(name = "action.hsm_restore", skip(self, ctx), fields(fid = %entry.fid))]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        if entry.kind != EntryKind::File {
            return Ok(ActionOutcome::Skipped {
                reason: "HSM restore only applies to files".to_string(),
            });
        }

        let path = fid_path(&ctx.mount_path, entry);
        let state = {
            let p = path.clone();
            let lustre = ctx.lustre;
            tokio::task::spawn_blocking(move || lustre.hsm_state_get(&p))
                .await
                .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??
        };

        if !state.states.contains(lustre_api::hsm::HsmState::RELEASED) {
            return Ok(ActionOutcome::Skipped {
                reason: "file is not released — restore not needed".to_string(),
            });
        }

        let fid = entry.fid;
        let mount = ctx.mount_path.clone();
        let lustre = ctx.lustre;
        tokio::task::spawn_blocking(move || {
            HsmRequestBuilder::new(HsmAction::Restore)
                .add_fid(fid)
                .submit(&lustre, &mount)
        })
        .await
        .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??;

        tracing::info!("HSM restore request submitted");
        Ok(ActionOutcome::Success)
    }
}

// ---------------------------------------------------------------------------
// HSM Remove — drives PolicyKind::HsmRemove
// ---------------------------------------------------------------------------

/// Submits an HSM remove request to delete the backend copy of an archived file.
///
/// The Lustre-side file is NOT deleted. After this action the file's HSM
/// state returns to `0x00000000` (no archive copy). Only acts on files that
/// are `archived` and NOT `dirty` — dirty files still have pending changes
/// that haven't been re-archived yet.
pub struct HsmRemoveExecutor;

#[async_trait]
impl ActionExecutor for HsmRemoveExecutor {
    #[tracing::instrument(name = "action.hsm_remove", skip(self, ctx), fields(fid = %entry.fid))]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        if entry.kind != EntryKind::File {
            return Ok(ActionOutcome::Skipped {
                reason: "HSM remove only applies to files".to_string(),
            });
        }

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
                reason: "file has no HSM archive copy — nothing to remove".to_string(),
            });
        }
        if state.states.contains(lustre_api::hsm::HsmState::DIRTY) {
            return Ok(ActionOutcome::Skipped {
                reason: "file is dirty (modified after archive) — re-archive first".to_string(),
            });
        }

        let fid = entry.fid;
        let mount = ctx.mount_path.clone();
        let lustre = ctx.lustre;
        tokio::task::spawn_blocking(move || {
            HsmRequestBuilder::new(HsmAction::Remove)
                .add_fid(fid)
                .submit(&lustre, &mount)
        })
        .await
        .map_err(|e| ActionError::Store(format!("spawn_blocking join error: {e}")))??;

        tracing::info!("HSM remove request submitted");
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

// ---------------------------------------------------------------------------
// Cmd (arbitrary subprocess) — drives PolicyKind::Migration
// ---------------------------------------------------------------------------

/// Runs an operator-defined shell command, one invocation per entry.
///
/// Built-in template placeholders:
///   `{fid}`      — literal FID, e.g. `[0x200000401:0x42:0x0]`
///   `{path}`     — `.lustre/fid/<FID>` virtual path (for ioctl/open)
///   `{fullpath}` — real POSIX path via fid_to_path (for rename/unlink/migrate)
///   `{size}`, `{uid}`, `{gid}`, `{mtime}`, `{ctime}`, `{atime}`
///
/// Custom placeholders: any key in `cmd_vars` becomes `{key}`.
/// Mirrors robinhood-C's `action_params { target_pool = hddpool; }`.
///
/// Non-zero exit codes are surfaced as [`ActionOutcome::Failed`].
/// STDERR (up to 4 KiB) is captured into the failure reason.
pub struct CmdExecutor {
    pub command: std::path::PathBuf,
    pub args_template: Vec<String>,
    pub timeout: Option<std::time::Duration>,
    /// Operator-defined substitution variables (from `cmd_vars` in policy JSON).
    pub cmd_vars: std::collections::HashMap<String, String>,
}

impl CmdExecutor {
    pub fn new(
        command: impl Into<std::path::PathBuf>, args: Vec<String>, timeout_secs: Option<u64>,
        cmd_vars: std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            command: command.into(),
            args_template: args,
            timeout: timeout_secs.map(std::time::Duration::from_secs),
            cmd_vars,
        }
    }

    /// Render one arg string, substituting all built-in and custom placeholders.
    /// `fullpath` is resolved lazily — pass `None` if fid_to_path is not available.
    fn render_arg(&self, template: &str, entry: &EntryRow, fid_path_s: &str, fullpath: &str) -> String {
        let fid_str = entry.fid.to_string();
        let mut out = template
            .replace("{fid}", &fid_str)
            .replace("{path}", fid_path_s)
            .replace("{fullpath}", fullpath)
            .replace("{size}", &entry.size.to_string())
            .replace("{uid}", &entry.uid.to_string())
            .replace("{gid}", &entry.gid.to_string())
            .replace("{mtime}", &entry.mtime.to_string())
            .replace("{ctime}", &entry.ctime.to_string())
            .replace("{atime}", &entry.atime.to_string());
        for (k, v) in &self.cmd_vars {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }

    fn render_args(&self, entry: &EntryRow, mount: &Path, lustre: &LustreApi) -> Vec<String> {
        let fid_str = entry.fid.to_string();
        let stripped = fid_str.trim_start_matches('[').trim_end_matches(']');
        let fid_vpath = mount.join(".lustre").join("fid").join(stripped);
        let fid_path_s = fid_vpath.to_string_lossy().into_owned();

        // Resolve real path only if any template needs {fullpath}.
        let needs_fullpath = self.args_template.iter().any(|t| t.contains("{fullpath}"));
        let fullpath = if needs_fullpath {
            resolve_real_path(lustre, mount, &entry.fid)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };

        self.args_template
            .iter()
            .map(|t| self.render_arg(t, entry, &fid_path_s, &fullpath))
            .collect()
    }
}

#[async_trait]
impl ActionExecutor for CmdExecutor {
    #[tracing::instrument(name = "action.cmd", skip(self, ctx), fields(fid = %entry.fid))]
    async fn execute(&self, entry: &EntryRow, ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        let args = self.render_args(entry, &ctx.mount_path, &ctx.lustre);
        let child_res = tokio::process::Command::new(&self.command)
            .args(&args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null())
            .spawn();
        let child = match child_res {
            Ok(c) => c,
            Err(e) => return Err(ActionError::Io(e)),
        };
        let output = match self.timeout {
            Some(t) => match tokio::time::timeout(t, child.wait_with_output()).await {
                Ok(r) => r.map_err(ActionError::Io)?,
                Err(_) => {
                    return Ok(ActionOutcome::Failed {
                        error: format!("timeout after {t:?}"),
                    });
                }
            },
            None => child.wait_with_output().await.map_err(ActionError::Io)?,
        };
        if output.status.success() {
            Ok(ActionOutcome::Success)
        } else {
            let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.len() > 4096 {
                stderr.truncate(4096);
            }
            Ok(ActionOutcome::Failed {
                error: format!("exit {:?}: {stderr}", output.status.code()),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Alert — drives PolicyKind::Alert
// ---------------------------------------------------------------------------

/// Emits an alert per matched entry. If `webhook` is configured, POSTs
/// a JSON payload with the entry's core metadata; if `log` is true,
/// also emits a tracing::warn! record. Webhook failures are surfaced
/// as `ActionOutcome::Failed` but do NOT propagate as errors — the
/// policy run continues.
pub struct AlertExecutor {
    pub webhook: Option<String>,
    pub log: bool,
    pub message: Option<String>,
    pub client: reqwest::Client,
}

impl AlertExecutor {
    pub fn new(webhook: Option<String>, log: bool, message: Option<String>) -> Self {
        Self {
            webhook,
            log,
            message,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }

    fn payload(&self, entry: &EntryRow) -> serde_json::Value {
        serde_json::json!({
            "fid": entry.fid.to_string(),
            "size": entry.size,
            "uid": entry.uid,
            "gid": entry.gid,
            "mtime": entry.mtime,
            "atime": entry.atime,
            "message": self.message,
        })
    }
}

#[async_trait]
impl ActionExecutor for AlertExecutor {
    #[tracing::instrument(name = "action.alert", skip(self, _ctx), fields(fid = %entry.fid))]
    async fn execute(&self, entry: &EntryRow, _ctx: &ActionContext) -> Result<ActionOutcome, ActionError> {
        if self.log {
            tracing::warn!(
                fid = %entry.fid,
                size = entry.size,
                uid = entry.uid,
                message = self.message.as_deref().unwrap_or(""),
                "alert fired"
            );
        }
        if let Some(url) = &self.webhook {
            let payload = self.payload(entry);
            match self.client.post(url).json(&payload).send().await {
                Ok(r) if r.status().is_success() => Ok(ActionOutcome::Success),
                Ok(r) => Ok(ActionOutcome::Failed {
                    error: format!("webhook {}: HTTP {}", url, r.status()),
                }),
                Err(e) => Ok(ActionOutcome::Failed {
                    error: format!("webhook {url}: {e}"),
                }),
            }
        } else {
            Ok(ActionOutcome::Success)
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
            depth: 3,
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

    #[test]
    fn cmd_template_renders_all_placeholders() {
        let entry = test_file_entry();
        let exec = CmdExecutor::new(
            "/bin/echo",
            vec![
                "{fid}".into(),
                "{path}".into(),
                "{size}".into(),
                "{uid}".into(),
                "{gid}".into(),
                "{mtime}".into(),
            ],
            None,
            std::collections::HashMap::new(),
        );
        let args = exec.render_args(&entry, Path::new("/lustre"), &LustreApi);
        assert!(args[0].starts_with("["), "fid literal: {}", args[0]);
        assert!(args[1].contains("/lustre/.lustre/fid/"), "path: {}", args[1]);
        assert_eq!(args[2], "1024");
        assert_eq!(args[3], "1000");
        assert_eq!(args[4], "100");
        assert_eq!(args[5], entry.mtime.to_string());
    }

    #[tokio::test]
    async fn cmd_success_returns_success() {
        let entry = test_file_entry();
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        let exec = CmdExecutor::new("/bin/true", vec![], None, std::collections::HashMap::new());
        assert_eq!(exec.execute(&entry, &ctx).await.unwrap(), ActionOutcome::Success);
    }

    #[tokio::test]
    async fn cmd_failure_returns_failed() {
        let entry = test_file_entry();
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        let exec = CmdExecutor::new("/bin/false", vec![], None, std::collections::HashMap::new());
        let outcome = exec.execute(&entry, &ctx).await.unwrap();
        assert!(matches!(outcome, ActionOutcome::Failed { .. }));
    }

    #[tokio::test]
    async fn cmd_timeout_returns_failed() {
        let entry = test_file_entry();
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        let exec = CmdExecutor::new(
            "/bin/sleep",
            vec!["5".into()],
            Some(1),
            std::collections::HashMap::new(),
        );
        // Use try_for_1s via timeout_secs=1; /bin/sleep 5 will time out.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), exec.execute(&entry, &ctx))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ActionOutcome::Failed { .. }));
    }

    #[tokio::test]
    async fn alert_log_only_succeeds() {
        let entry = test_file_entry();
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        let exec = AlertExecutor::new(None, true, Some("high mtime age".into()));
        assert_eq!(exec.execute(&entry, &ctx).await.unwrap(), ActionOutcome::Success);
    }

    #[tokio::test]
    async fn alert_webhook_unreachable_returns_failed() {
        let entry = test_file_entry();
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        // 127.0.0.1:1 should always refuse. The executor captures the
        // connection error into Failed, not Err.
        let exec = AlertExecutor::new(Some("http://127.0.0.1:1/".into()), false, None);
        let outcome = exec.execute(&entry, &ctx).await.unwrap();
        assert!(matches!(outcome, ActionOutcome::Failed { .. }), "got {outcome:?}");
    }

    // -----------------------------------------------------------------------
    // CmdExecutor: cmd_vars custom template substitution
    // -----------------------------------------------------------------------

    #[test]
    fn cmd_vars_substitute_custom_placeholders() {
        let entry = test_file_entry();
        let mut vars = std::collections::HashMap::new();
        vars.insert("target_pool".into(), "hddpool".into());
        vars.insert("stripe_count".into(), "-1".into());
        let exec = CmdExecutor::new(
            "/usr/bin/lfs",
            vec![
                "migrate".into(),
                "-c".into(),
                "{stripe_count}".into(),
                "-p".into(),
                "{target_pool}".into(),
                // {fullpath} resolves via fid_to_path; in unit tests it
                // returns an error so we get an empty string — just verify
                // substitution itself doesn't panic.
                "{fullpath}".into(),
            ],
            None,
            vars,
        );
        let args = exec.render_args(&entry, Path::new("/lustre"), &LustreApi);
        assert_eq!(args[0], "migrate");
        assert_eq!(args[2], "-1", "stripe_count var");
        assert_eq!(args[4], "hddpool", "target_pool var");
        // {fullpath} on mock LustreApi fails fid_to_path → empty string (not panics)
        assert_eq!(args[5], "", "fullpath falls back to empty on fid_to_path failure");
    }

    #[test]
    fn cmd_vars_unknown_placeholder_left_as_is() {
        let entry = test_file_entry();
        let exec = CmdExecutor::new(
            "/bin/echo",
            vec!["{unknown_key}".into()],
            None,
            std::collections::HashMap::new(),
        );
        let args = exec.render_args(&entry, Path::new("/lustre"), &LustreApi);
        assert_eq!(
            args[0], "{unknown_key}",
            "unknown placeholder must not be silently dropped"
        );
    }

    // -----------------------------------------------------------------------
    // HsmRestoreExecutor / HsmRemoveExecutor: state-guard skips
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn hsm_restore_skips_non_file_entries() {
        use rbh_entry_store::model::EntryKind;
        let mut entry = test_file_entry();
        entry.kind = EntryKind::Directory;
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        let exec = HsmRestoreExecutor;
        let out = exec.execute(&entry, &ctx).await.unwrap();
        assert!(matches!(out, ActionOutcome::Skipped { .. }), "dirs must be skipped");
    }

    #[tokio::test]
    async fn hsm_remove_skips_non_file_entries() {
        use rbh_entry_store::model::EntryKind;
        let mut entry = test_file_entry();
        entry.kind = EntryKind::Directory;
        let ctx = ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: LustreApi,
        };
        let exec = HsmRemoveExecutor;
        let out = exec.execute(&entry, &ctx).await.unwrap();
        assert!(matches!(out, ActionOutcome::Skipped { .. }), "dirs must be skipped");
    }
}
