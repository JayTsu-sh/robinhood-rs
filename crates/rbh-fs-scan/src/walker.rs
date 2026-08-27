//! Lustre enrichment Adapter over the backend-neutral POSIX walker.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lustre_api::LustreApi;
use rbh_entry_store::model::EntryRow;
use tokio::sync::mpsc;

use crate::entry::enrich_lustre;
use crate::posix::{PosixWalkEvent, PosixWalker};

#[derive(Debug)]
pub enum ScanEvent {
    Entry(Box<EntryRow>),
    Error { path: String, error: String },
}

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub root: PathBuf,
    pub concurrency: usize,
    pub max_depth: Option<usize>,
    pub channel_size: usize,
    pub since_mtime: Option<i64>,
    pub ignore_globs: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/lustre"),
            concurrency: std::thread::available_parallelism()
                .map(|count| count.get().min(16))
                .unwrap_or(4),
            max_depth: None,
            channel_size: 4096,
            since_mtime: None,
            ignore_globs: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct ScanProgress {
    pub entries_scanned: AtomicU64,
    pub errors: AtomicU64,
    pub dirs_walked: AtomicU64,
}

impl ScanProgress {
    pub(crate) fn new() -> Self {
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

/// Compatibility entry point for a full Lustre scan.
pub struct FsScanner;

impl FsScanner {
    #[tracing::instrument(skip_all, fields(root = %config.root.display()))]
    pub fn run(config: ScanConfig) -> (mpsc::Receiver<ScanEvent>, Arc<ScanProgress>) {
        let channel_size = config.channel_size;
        let enrichment_workers = config.concurrency.max(1);
        let (posix_events, progress) = PosixWalker::run(config);
        let (event_tx, event_rx) = mpsc::channel(channel_size);
        let output_progress = progress.clone();
        let posix_events = Arc::new(tokio::sync::Mutex::new(posix_events));
        for worker_id in 0..enrichment_workers {
            let posix_events = posix_events.clone();
            let event_tx = event_tx.clone();
            let progress = progress.clone();
            tokio::spawn(async move {
                let lustre = LustreApi;
                loop {
                    let event = posix_events.lock().await.recv().await;
                    let Some(event) = event else { break };
                    let event = match event {
                        PosixWalkEvent::Entry(entry) => {
                            let path = entry.path.display().to_string();
                            let result = tokio::task::spawn_blocking(move || enrich_lustre(&lustre, &entry)).await;
                            match result {
                                Ok(Ok(entry)) => ScanEvent::Entry(Box::new(entry)),
                                Ok(Err(error)) => {
                                    progress.entries_scanned.fetch_sub(1, Ordering::Relaxed);
                                    progress.errors.fetch_add(1, Ordering::Relaxed);
                                    ScanEvent::Error {
                                        path,
                                        error: error.to_string(),
                                    }
                                }
                                Err(error) => {
                                    progress.entries_scanned.fetch_sub(1, Ordering::Relaxed);
                                    progress.errors.fetch_add(1, Ordering::Relaxed);
                                    ScanEvent::Error {
                                        path,
                                        error: format!("Lustre enrichment task failed: {error}"),
                                    }
                                }
                            }
                        }
                        PosixWalkEvent::Error { path, error } => ScanEvent::Error { path, error },
                    };
                    if event_tx.send(event).await.is_err() {
                        break;
                    }
                }
                tracing::debug!(worker_id, "Lustre enrichment worker finished");
            });
        }
        drop(event_tx);
        (event_rx, output_progress)
    }
}

pub fn load_rbh_ignore_file(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(".rbh_ignore"))
        .map(|contents| {
            contents
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn glob_matches(pattern: &str, value: &str) -> bool {
    fn matches(pattern: &[u8], value: &[u8]) -> bool {
        if pattern.is_empty() {
            return value.is_empty();
        }
        match pattern[0] {
            b'*' => (0..=value.len()).any(|offset| matches(&pattern[1..], &value[offset..])),
            b'?' => !value.is_empty() && matches(&pattern[1..], &value[1..]),
            b'[' => {
                let Some(end) = pattern.iter().position(|byte| *byte == b']') else {
                    return false;
                };
                !value.is_empty() && pattern[1..end].contains(&value[0]) && matches(&pattern[end + 1..], &value[1..])
            }
            byte => !value.is_empty() && value[0] == byte && matches(&pattern[1..], &value[1..]),
        }
    }
    matches(pattern.as_bytes(), value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns_match_names() {
        assert!(glob_matches("*.tmp", "scratch.tmp"));
        assert!(glob_matches("f?le", "file"));
        assert!(glob_matches("[abc].tmp", "a.tmp"));
        assert!(!glob_matches("[abc].tmp", "d.tmp"));
    }

    #[test]
    fn ignore_file_is_parsed() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".rbh_ignore"), "# comment\n\n*.tmp\n core.* \n").unwrap();
        assert_eq!(load_rbh_ignore_file(root.path()), vec!["*.tmp", "core.*"]);
    }

    #[tokio::test]
    async fn nonexistent_root_reports_error() {
        let (mut events, progress) = PosixWalker::run(ScanConfig {
            root: PathBuf::from("/nonexistent-rbh-posix-root"),
            concurrency: 1,
            ..ScanConfig::default()
        });
        assert!(matches!(events.recv().await, Some(PosixWalkEvent::Error { .. })));
        assert!(progress.snapshot().1 > 0);
    }
}
