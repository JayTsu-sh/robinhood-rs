//! Async filesystem walker using `async_channel` + `AtomicUsize` pending counter.
//!
//! Spawns N worker tasks that pull directory paths from a shared MPMC queue,
//! read each directory, stat children, build [`EntryRow`]s, and emit
//! [`ScanEvent`]s through a bounded channel. Termination is correct by
//! construction: when `pending` hits zero, the work channel is closed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use lustre_api::LustreApi;
use lustre_api::fid::LuFid;
use rbh_entry_store::model::EntryRow;
use tokio::sync::mpsc;

use crate::ScanError;
use crate::entry::build_entry;

/// Events emitted during a scan.
#[derive(Debug)]
pub enum ScanEvent {
    /// A successfully scanned entry.
    Entry(Box<EntryRow>),
    /// A non-fatal error for a single path.
    Error { path: String, error: String },
}

/// Scan configuration.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Root directory to scan (Lustre mount point).
    pub root: PathBuf,
    /// Number of concurrent worker tasks.
    pub concurrency: usize,
    /// Maximum directory depth (0 = root only, None = unlimited).
    pub max_depth: Option<usize>,
    /// Channel buffer size for scan events.
    pub channel_size: usize,
    /// Incremental scan: when set, entries whose `mtime < since_mtime`
    /// are skipped. Directories are still descended (their contents
    /// may be newer), but individual files below the threshold are not
    /// emitted. Unit is unix seconds.
    pub since_mtime: Option<i64>,
    /// Pattern list matched against the entry *name* (not full path).
    /// Shell globs — `*`, `?`, `[abc]`. Matches cause the entry to be
    /// skipped entirely (directories skipped with this also pruned
    /// from descent). Loaded from `.rbh_ignore` by default when the
    /// walker discovers one at the scan root.
    pub ignore_globs: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/lustre"),
            concurrency: num_workers(),
            max_depth: None,
            channel_size: 4096,
            since_mtime: None,
            ignore_globs: Vec::new(),
        }
    }
}

fn num_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(16))
        .unwrap_or(4)
}

/// Scan progress counters (shared across workers).
#[derive(Debug)]
pub struct ScanProgress {
    pub entries_scanned: AtomicU64,
    pub errors: AtomicU64,
    pub dirs_walked: AtomicU64,
}

impl ScanProgress {
    fn new() -> Self {
        Self {
            entries_scanned: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            dirs_walked: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.entries_scanned.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.dirs_walked.load(Ordering::Relaxed),
        )
    }
}

/// Async filesystem scanner.
pub struct FsScanner;

impl FsScanner {
    /// Start a scan and return a receiver of [`ScanEvent`]s plus shared progress.
    ///
    /// Spawns `config.concurrency` worker tasks that walk the filesystem.
    /// The returned `mpsc::Receiver` yields events until the scan completes.
    #[tracing::instrument(skip_all, fields(root = %config.root.display(), concurrency = config.concurrency))]
    pub fn run(config: ScanConfig) -> (mpsc::Receiver<ScanEvent>, Arc<ScanProgress>) {
        let (event_tx, event_rx) = mpsc::channel(config.channel_size);
        let progress = Arc::new(ScanProgress::new());

        // Work queue: directories to process, with their depth and parent FID.
        let (work_tx, work_rx) = async_channel::unbounded::<(PathBuf, usize, Option<LuFid>)>();

        // Pending counter: tracks how many directories are queued or being processed.
        let pending = Arc::new(AtomicUsize::new(1)); // 1 for the root

        // Seed the root directory.
        let _ = work_tx.try_send((config.root.clone(), 0, None));

        // Merge ignore globs from .rbh_ignore at the scan root (if any)
        // with globs supplied via config. The file format mirrors
        // .gitignore: one glob per line, `#` lines and blanks are
        // ignored.
        let mut ignore = config.ignore_globs.clone();
        ignore.extend(load_rbh_ignore_file(&config.root));

        let state = Arc::new(WalkState {
            lustre: LustreApi,
            max_depth: config.max_depth,
            work_tx: work_tx.clone(),
            event_tx: event_tx.clone(),
            pending: pending.clone(),
            progress: progress.clone(),
            since_mtime: config.since_mtime,
            ignore_globs: Arc::new(ignore),
        });

        for worker_id in 0..config.concurrency {
            let work_rx = work_rx.clone();
            let state = state.clone();

            tokio::task::spawn(async move {
                tracing::debug!(worker_id, "scan worker started");

                while let Ok((dir_path, depth, parent_fid)) = work_rx.recv().await {
                    process_directory(&state, &dir_path, depth, parent_fid).await;

                    if state.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
                        state.work_tx.close();
                    }
                }

                tracing::debug!(worker_id, "scan worker finished");
            });
        }

        drop(work_tx);
        drop(event_tx);

        (event_rx, progress)
    }
}

/// Shared state passed to `process_directory` to avoid too many arguments.
struct WalkState {
    lustre: LustreApi,
    max_depth: Option<usize>,
    work_tx: async_channel::Sender<(PathBuf, usize, Option<LuFid>)>,
    event_tx: mpsc::Sender<ScanEvent>,
    pending: Arc<AtomicUsize>,
    progress: Arc<ScanProgress>,
    /// Skip entries whose mtime is older than this (unix seconds).
    since_mtime: Option<i64>,
    /// Glob patterns matched against the entry name (not full path).
    ignore_globs: Arc<Vec<String>>,
}

/// Parse `.rbh_ignore` at the scan root. Returns empty on IO error —
/// missing file is a common case, don't propagate.
pub fn load_rbh_ignore_file(root: &Path) -> Vec<String> {
    let path = root.join(".rbh_ignore");
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    contents
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Shell-glob match: `*` matches zero-or-more, `?` exactly one, `[abc]`
/// a character class. Anchored at both ends (i.e. the whole name must
/// match the pattern).
pub(crate) fn glob_matches(pat: &str, s: &str) -> bool {
    // Recursive walk; patterns are short so this is fine.
    fn m(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => {
                // Try consuming zero or more of t.
                for i in 0..=t.len() {
                    if m(&p[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !t.is_empty() && m(&p[1..], &t[1..]),
            b'[' => {
                // Find closing ].
                let mut end = 1;
                while end < p.len() && p[end] != b']' {
                    end += 1;
                }
                if end == p.len() || t.is_empty() {
                    return false;
                }
                let class = &p[1..end];
                if class.contains(&t[0]) {
                    m(&p[end + 1..], &t[1..])
                } else {
                    false
                }
            }
            c => !t.is_empty() && t[0] == c && m(&p[1..], &t[1..]),
        }
    }
    m(pat.as_bytes(), s.as_bytes())
}

fn is_ignored(globs: &[String], name: &str) -> bool {
    globs.iter().any(|g| glob_matches(g, name))
}

async fn process_directory(state: &WalkState, dir_path: &Path, depth: usize, parent_fid: Option<LuFid>) {
    state.progress.dirs_walked.fetch_add(1, Ordering::Relaxed);

    // First, build an entry for the directory itself.
    let dir_fid = match build_entry_blocking(&state.lustre, dir_path, parent_fid, depth as u32).await {
        Ok(entry) => {
            let fid = entry.fid;
            state.progress.entries_scanned.fetch_add(1, Ordering::Relaxed);
            let _ = state.event_tx.send(ScanEvent::Entry(Box::new(entry))).await;
            Some(fid)
        }
        Err(e) => {
            state.progress.errors.fetch_add(1, Ordering::Relaxed);
            let _ = state
                .event_tx
                .send(ScanEvent::Error {
                    path: dir_path.display().to_string(),
                    error: e.to_string(),
                })
                .await;
            None
        }
    };

    // Read directory entries.
    let mut read_dir = match tokio::fs::read_dir(dir_path).await {
        Ok(rd) => rd,
        Err(e) => {
            state.progress.errors.fetch_add(1, Ordering::Relaxed);
            let _ = state
                .event_tx
                .send(ScanEvent::Error {
                    path: dir_path.display().to_string(),
                    error: format!("readdir failed: {e}"),
                })
                .await;
            return;
        }
    };

    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let child_path = entry.path();

        // .rbh_ignore + inline globs: match on file name.
        let child_name_os = entry.file_name();
        let child_name = child_name_os.to_string_lossy();
        if is_ignored(&state.ignore_globs, &child_name) {
            continue;
        }

        // Stat child to determine type.
        let file_type = match entry.file_type().await {
            Ok(ft) => ft,
            Err(e) => {
                state.progress.errors.fetch_add(1, Ordering::Relaxed);
                let _ = state
                    .event_tx
                    .send(ScanEvent::Error {
                        path: child_path.display().to_string(),
                        error: format!("file_type failed: {e}"),
                    })
                    .await;
                continue;
            }
        };

        if file_type.is_dir() {
            // Enqueue subdirectory if within depth limit.
            if state.max_depth.is_none_or(|max| depth < max) {
                state.pending.fetch_add(1, Ordering::AcqRel);
                if state.work_tx.send((child_path, depth + 1, dir_fid)).await.is_err() {
                    state.pending.fetch_sub(1, Ordering::AcqRel);
                }
            }
        } else {
            // Non-directory: stat, resolve FID, emit entry.
            match build_entry_blocking(&state.lustre, &child_path, dir_fid, depth as u32 + 1).await {
                Ok(row) => {
                    // Incremental scan: drop entries older than the cutoff.
                    // Directories are kept (their children may be newer);
                    // this filter applies only to files / symlinks / etc.
                    if let Some(cut) = state.since_mtime
                        && row.mtime < cut
                    {
                        continue;
                    }
                    state.progress.entries_scanned.fetch_add(1, Ordering::Relaxed);
                    let _ = state.event_tx.send(ScanEvent::Entry(Box::new(row))).await;
                }
                Err(e) => {
                    state.progress.errors.fetch_add(1, Ordering::Relaxed);
                    let _ = state
                        .event_tx
                        .send(ScanEvent::Error {
                            path: child_path.display().to_string(),
                            error: e.to_string(),
                        })
                        .await;
                }
            }
        }
    }
}

/// Run `build_entry` on a blocking thread (stat + FFI are sync).
async fn build_entry_blocking(
    lustre: &LustreApi, path: &Path, parent_fid: Option<LuFid>, depth: u32,
) -> Result<EntryRow, ScanError> {
    let lustre = *lustre; // Copy (LustreApi is Copy)
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || build_entry(&lustre, &path, parent_fid, depth))
        .await
        .map_err(|e| ScanError::Io {
            path: String::new(),
            source: std::io::Error::other(e.to_string()),
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn scan_config_defaults() {
        let cfg = ScanConfig::default();
        assert_eq!(cfg.root, Path::new("/lustre"));
        assert!(cfg.concurrency >= 1 && cfg.concurrency <= 16);
        assert!(cfg.max_depth.is_none());
        assert_eq!(cfg.channel_size, 4096);
    }

    #[test]
    fn progress_snapshot_starts_zero() {
        let p = ScanProgress::new();
        let (scanned, errors, dirs) = p.snapshot();
        assert_eq!(scanned, 0);
        assert_eq!(errors, 0);
        assert_eq!(dirs, 0);
    }

    #[tokio::test]
    async fn scan_nonexistent_root_emits_error() {
        let config = ScanConfig {
            root: PathBuf::from("/nonexistent_fs_scan_test_root"),
            concurrency: 1,
            max_depth: Some(0),
            channel_size: 16,
            since_mtime: None,
            ignore_globs: Vec::new(),
        };
        let (mut rx, progress) = FsScanner::run(config);

        let mut got_error = false;
        while let Some(event) = rx.recv().await {
            if let ScanEvent::Error { .. } = event {
                got_error = true;
            }
        }
        assert!(got_error);
        let (_, errors, _) = progress.snapshot();
        assert!(errors > 0);
    }

    #[tokio::test]
    async fn scan_tmp_finds_entries() {
        // Scan /tmp with depth 0 — should find /tmp itself.
        let config = ScanConfig {
            root: PathBuf::from("/tmp"),
            concurrency: 1,
            max_depth: Some(0),
            channel_size: 64,
            since_mtime: None,
            ignore_globs: Vec::new(),
        };
        let (mut rx, progress) = FsScanner::run(config);

        while let Some(_event) = rx.recv().await {
            // On non-Lustre systems, path_to_fid will fail → errors
            // That's expected; we're testing the walker mechanics
        }
        // Either we got entries (Lustre) or errors (non-Lustre), but something happened
        let (scanned, errors, _) = progress.snapshot();
        assert!(scanned > 0 || errors > 0);
    }

    #[test]
    fn glob_literal_and_star() {
        assert!(glob_matches("foo.txt", "foo.txt"));
        assert!(!glob_matches("foo.txt", "bar.txt"));
        assert!(glob_matches("*.tmp", "scratch.tmp"));
        assert!(glob_matches("*.tmp", ".tmp"));
        assert!(!glob_matches("*.tmp", "scratch.log"));
        assert!(glob_matches("core.*", "core.1234"));
    }

    #[test]
    fn glob_question_and_class() {
        assert!(glob_matches("f?le", "file"));
        assert!(glob_matches("f?le", "fale"));
        assert!(!glob_matches("f?le", "fle"));
        assert!(glob_matches("[abc].tmp", "a.tmp"));
        assert!(!glob_matches("[abc].tmp", "d.tmp"));
    }

    #[test]
    fn load_rbh_ignore_reads_and_strips() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(dir.path().join(".rbh_ignore")).unwrap();
        writeln!(f, "# comment").unwrap();
        writeln!(f).unwrap();
        writeln!(f, "*.tmp").unwrap();
        writeln!(f, "   core.*   ").unwrap();
        drop(f);
        let globs = load_rbh_ignore_file(dir.path());
        assert_eq!(globs, vec!["*.tmp".to_string(), "core.*".to_string()]);
    }

    #[test]
    fn load_rbh_ignore_missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_rbh_ignore_file(dir.path()).is_empty());
    }
}
