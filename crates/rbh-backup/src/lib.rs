//! External backup adapters for robinhood-rs policies.
//!
//! robinhood-C shipped `scripts/rbhext_tool*` and `src/modules/backup.c`
//! to let operators plug arbitrary copy tools behind archive / restore /
//! remove actions. This crate replicates the rbhext_tool protocol in
//! Rust: a simple trait plus a subprocess adapter that invokes a
//! configurable command template.
//!
//! The contract (matching the bash scripts under `scripts/`):
//!
//! ```text
//!   TOOL ARCHIVE <src> <dest> [hints]     # -> exit 0 on success
//!   TOOL RESTORE <src> <dest> [hints]
//!   TOOL REMOVE  <src>         [hints]    # robinhood-rs extension
//! ```
//!
//! Non-zero exit is surfaced as [`BackupError::ToolFailed`]. STDERR is
//! captured and attached to the error.
//!
//! Policies configure an adapter via [`BackupCommandConfig`] on
//! `rbh_policy::ActionParams.backup` (added separately to avoid a
//! dependency cycle; see `rbh-policy::BackupConfig`).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

pub mod protocol;

pub use protocol::{BackupOp, ToolInvocation};

/// Errors raised by an adapter.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backup tool exited with code {code}: {stderr}")]
    ToolFailed { code: i32, stderr: String },
    #[error("backup tool killed by signal")]
    ToolSignaled,
    #[error("I/O while invoking backup tool: {0}")]
    Io(#[from] std::io::Error),
    #[error("config error: {0}")]
    Config(String),
}

/// Abstraction over an external copy tool. Implementations translate
/// `src`/`dest`/`hints` into whatever the underlying tool expects
/// (subprocess, REST, RPC, …).
#[async_trait]
pub trait BackupAdapter: Send + Sync {
    async fn archive(&self, op: &ToolInvocation<'_>) -> Result<(), BackupError>;
    async fn restore(&self, op: &ToolInvocation<'_>) -> Result<(), BackupError>;
    async fn remove(&self, op: &ToolInvocation<'_>) -> Result<(), BackupError>;
}

/// Subprocess-based adapter matching rbhext_tool's argv contract.
///
/// The configured `command` is resolved on PATH; subsequent tokens in
/// `args_template` are substituted per-invocation with:
///
///   `{verb}` → `ARCHIVE` / `RESTORE` / `REMOVE`
///   `{src}`  → absolute source path (Lustre)
///   `{dest}` → destination path (backend)
///   `{hints}` → freeform string, empty when absent
///   `{archive_id}` → HSM archive id (when applicable, else `0`)
///
/// Default template: `["{verb}", "{src}", "{dest}", "{hints}"]`.
pub struct CommandBackupAdapter {
    pub command: PathBuf,
    pub args_template: Vec<String>,
    /// Max wall-clock per invocation. `None` = no timeout.
    pub timeout: Option<std::time::Duration>,
}

impl CommandBackupAdapter {
    pub fn new(command: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            args_template: vec!["{verb}".into(), "{src}".into(), "{dest}".into(), "{hints}".into()],
            timeout: None,
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args_template = args;
        self
    }

    pub fn with_timeout(mut self, d: std::time::Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    async fn run(&self, verb: &str, op: &ToolInvocation<'_>) -> Result<(), BackupError> {
        let rendered: Vec<String> = self.args_template.iter().map(|arg| render_arg(arg, verb, op)).collect();

        tracing::debug!(
            cmd = %self.command.display(),
            args = ?rendered,
            src = %op.src.display(),
            dest = ?op.dest.as_ref().map(|p| p.display().to_string()),
            "invoking backup tool"
        );

        let child = Command::new(&self.command)
            .args(&rendered)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()?;

        let output = match self.timeout {
            Some(t) => match tokio::time::timeout(t, child.wait_with_output()).await {
                Ok(res) => res?,
                Err(_) => {
                    // The child may still be alive; drop it best-effort.
                    return Err(BackupError::ToolFailed {
                        code: 124,
                        stderr: format!("timeout after {t:?}"),
                    });
                }
            },
            None => child.wait_with_output().await?,
        };

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            match output.status.code() {
                Some(code) => Err(BackupError::ToolFailed { code, stderr }),
                None => Err(BackupError::ToolSignaled),
            }
        }
    }
}

#[async_trait]
impl BackupAdapter for CommandBackupAdapter {
    async fn archive(&self, op: &ToolInvocation<'_>) -> Result<(), BackupError> {
        self.run("ARCHIVE", op).await
    }
    async fn restore(&self, op: &ToolInvocation<'_>) -> Result<(), BackupError> {
        self.run("RESTORE", op).await
    }
    async fn remove(&self, op: &ToolInvocation<'_>) -> Result<(), BackupError> {
        self.run("REMOVE", op).await
    }
}

pub(crate) fn render_arg(template: &str, verb: &str, op: &ToolInvocation<'_>) -> String {
    let mut out = template.to_string();
    out = out.replace("{verb}", verb);
    out = out.replace("{src}", &path_str(op.src));
    out = out.replace(
        "{dest}",
        op.dest.as_ref().map(|p| path_str(p)).unwrap_or_default().as_str(),
    );
    out = out.replace("{hints}", op.hints.unwrap_or(""));
    out = out.replace("{archive_id}", &op.archive_id.to_string());
    out
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_invocation<'a>() -> ToolInvocation<'a> {
        ToolInvocation {
            op: BackupOp::Archive,
            src: Path::new("/lustre/foo/bar.txt"),
            dest: Some(Path::new("/archive/bar.txt")),
            hints: Some("tier=cold"),
            archive_id: 1,
        }
    }

    #[test]
    fn render_substitutes_all_placeholders() {
        let inv = demo_invocation();
        assert_eq!(render_arg("{verb}", "ARCHIVE", &inv), "ARCHIVE");
        assert_eq!(render_arg("{src}", "ARCHIVE", &inv), "/lustre/foo/bar.txt");
        assert_eq!(render_arg("{dest}", "ARCHIVE", &inv), "/archive/bar.txt");
        assert_eq!(render_arg("{hints}", "ARCHIVE", &inv), "tier=cold");
        assert_eq!(render_arg("{archive_id}", "ARCHIVE", &inv), "1");
    }

    #[test]
    fn render_handles_empty_optional_fields() {
        let inv = ToolInvocation {
            op: BackupOp::Remove,
            src: Path::new("/lustre/x"),
            dest: None,
            hints: None,
            archive_id: 0,
        };
        assert_eq!(render_arg("{dest}={dest}", "REMOVE", &inv), "=");
        assert_eq!(render_arg("[{hints}]", "REMOVE", &inv), "[]");
    }

    #[tokio::test]
    async fn command_adapter_runs_true() {
        let adapter = CommandBackupAdapter::new("/bin/true");
        let inv = demo_invocation();
        assert!(adapter.archive(&inv).await.is_ok());
    }

    #[tokio::test]
    async fn command_adapter_surfaces_nonzero_exit() {
        let adapter = CommandBackupAdapter::new("/bin/false");
        let inv = demo_invocation();
        let err = adapter.restore(&inv).await.unwrap_err();
        assert!(matches!(err, BackupError::ToolFailed { code, .. } if code == 1));
    }

    #[tokio::test]
    async fn command_adapter_copies_file() {
        // Use /bin/cp as a real rbhext_tool. Template: cp -a {src} {dest}.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dest = dir.path().join("dest.txt");
        std::fs::write(&src, b"hello").unwrap();

        let adapter =
            CommandBackupAdapter::new("/bin/cp").with_args(vec!["-a".into(), "{src}".into(), "{dest}".into()]);
        let inv = ToolInvocation {
            op: BackupOp::Archive,
            src: &src,
            dest: Some(&dest),
            hints: None,
            archive_id: 0,
        };
        adapter.archive(&inv).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn command_adapter_times_out() {
        let adapter = CommandBackupAdapter::new("/bin/sleep")
            .with_args(vec!["5".into()])
            .with_timeout(std::time::Duration::from_millis(50));
        let inv = demo_invocation();
        let err = adapter.archive(&inv).await.unwrap_err();
        assert!(matches!(err, BackupError::ToolFailed { code: 124, .. }));
    }
}

/// Parse of a user-supplied backup tool configuration. Mirrors the
/// robinhood-C `backup_config { command = "..." timeout = Ns }` block
/// but in a shape that's easier to serialize as policy JSON.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BackupCommandConfig {
    pub command: String,
    /// Optional args template. Missing = default
    /// `["{verb}","{src}","{dest}","{hints}"]`.
    #[serde(default)]
    pub args: Option<Vec<String>>,
    /// Per-invocation timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Template for the backend destination path.
    /// Supports `{src}`, `{mount}`, `{archive_id}`.
    /// `None` → pass empty dest; the tool may derive it itself.
    #[serde(default)]
    pub dest_template: Option<String>,
}

impl BackupCommandConfig {
    pub fn build(&self) -> CommandBackupAdapter {
        let mut a = CommandBackupAdapter::new(&self.command);
        if let Some(args) = &self.args {
            a = a.with_args(args.clone());
        }
        if let Some(t) = self.timeout_secs {
            a = a.with_timeout(std::time::Duration::from_secs(t));
        }
        a
    }
}
