//! `PolicyRunTask` — scheduler-rs `Task` impl for action policy execution.
//!
//! At fire time the task:
//! 1. Loads the policy definition from `PolicyStore`.
//! 2. Builds a `Tags` predicate from `match_tags` (SQL pushdown).
//! 3. Queries candidate entries from `rbh-entry-store`.
//! 4. Dispatches all candidates to the action executor with uniform `ActionOpts`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use scheduler_rs::prelude::*;
use serde::{Deserialize, Serialize};

/// Shared runtime context injected once at daemon startup.
pub struct PolicyRuntime {
    pub policy_store: crate::PolicyStore,
    pub entry_store: rbh_entry_store::store::EntryStore,
    pub mount_path: PathBuf,
}

static RUNTIME: OnceLock<Arc<PolicyRuntime>> = OnceLock::new();

pub fn init_runtime(rt: Arc<PolicyRuntime>) {
    if RUNTIME.set(rt).is_err() {
        panic!("init_runtime called more than once — this is a startup bug");
    }
}

fn runtime() -> &'static Arc<PolicyRuntime> {
    RUNTIME
        .get()
        .expect("PolicyRuntime not initialized — call init_runtime() at startup")
}

/// Narrow a policy run to a specific subset of the filesystem.
/// Injected by threshold triggers at fire time.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetFilter {
    #[default]
    Fs,
    Ost {
        osts: Vec<u32>,
    },
    Pool {
        name: String,
    },
    User {
        uid: u32,
    },
    Group {
        gid: u32,
    },
    Projid {
        projid: u32,
    },
}

impl TargetFilter {
    pub fn to_predicate(&self) -> rbh_predicate::Predicate {
        use rbh_predicate::{CmpOp, Field, Predicate, Value};
        match self {
            Self::Fs => Predicate::True,
            Self::Ost { osts } => Predicate::OnOst { osts: osts.clone() },
            Self::Pool { name } => Predicate::InPool { pool: name.clone() },
            Self::User { uid } => Predicate::Cmp {
                field: Field::Uid,
                cmp: CmpOp::Eq,
                value: Value::Num(*uid as i64),
            },
            Self::Group { gid } => Predicate::Cmp {
                field: Field::Gid,
                cmp: CmpOp::Eq,
                value: Value::Num(*gid as i64),
            },
            Self::Projid { projid } => Predicate::Cmp {
                field: Field::Projid,
                cmp: CmpOp::Eq,
                value: Value::Num(*projid as i64),
            },
        }
    }
}

/// Scheduler-rs task payload for an action policy run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRunTask {
    pub policy_id: u64,
    /// Always 0 in the new design (single trigger per policy).
    #[serde(default)]
    pub trigger_idx: u32,
    #[serde(default)]
    pub target: TargetFilter,
    #[serde(default)]
    pub dry_run: bool,
}

#[async_trait]
impl Task for PolicyRunTask {
    const TYPE_NAME: &'static str = "rbh.policy_run";

    async fn run(&self, ctx: &TaskContext) -> TaskResult {
        let rt = runtime();
        let run_started = std::time::Instant::now();
        let pid_lbl = self.policy_id.to_string();

        tracing::info!(
            policy_id = self.policy_id,
            execution_id = %ctx.execution_id.0,
            "policy run started"
        );

        if ctx.cancellation_token.is_cancelled() {
            record_run(&pid_lbl, "cancelled", run_started);
            return Ok(());
        }

        // 1. Load policy definition.
        let policy = match rt.policy_store.get(self.policy_id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(policy_id = self.policy_id, error = %e, "failed to load policy");
                return Err(SchedulerError::ExecutionError(e.to_string()));
            }
        };
        let def = &policy.definition;
        tracing::info!(policy_id = self.policy_id, kind = def.kind.as_str(), "policy loaded");

        // 2. Get executor for this policy kind.
        let executor: Arc<dyn rbh_actions::ActionExecutor> = match make_executor(def) {
            Ok(e) => e,
            Err(reason) => {
                tracing::warn!(policy_id = self.policy_id, reason, "skipping misconfigured policy");
                record_run(&pid_lbl, "misconfigured", run_started);
                return Ok(());
            }
        };

        // 3. Build WHERE clause: Tags(match_tags) AND target_filter.
        let tags_pred = rbh_predicate::Predicate::Tags {
            match_tags: def.match_tags.clone(),
        };
        let target_pred = self.target.to_predicate();
        let effective = match target_pred {
            rbh_predicate::Predicate::True => tags_pred,
            other => rbh_predicate::Predicate::And {
                children: vec![tags_pred, other],
            },
        };
        let (where_clause, sql_params) = rbh_predicate::to_sql(&effective);
        let query_params: Vec<rbh_entry_store::store::QueryParam> = sql_params
            .into_iter()
            .map(|p| match p {
                rbh_predicate::SqlParam::Num(n) => rbh_entry_store::store::QueryParam::Int(n),
                rbh_predicate::SqlParam::Str(s) => rbh_entry_store::store::QueryParam::Str(s),
            })
            .collect();

        let max_count = def.action.max_count.unwrap_or(10_000);
        let order_by = def
            .action
            .lru_sort
            .and_then(|s| s.column())
            .map(|col| format!("{col} ASC"));

        let candidates = match rt
            .entry_store
            .query_page(&where_clause, &query_params, order_by.as_deref(), max_count, 0)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to query candidates");
                return Err(SchedulerError::ExecutionError(e.to_string()));
            }
        };

        tracing::info!(
            policy_id = self.policy_id,
            candidate_count = candidates.len(),
            "candidates queried"
        );

        // 4. Dispatch.
        let action_ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: rt.mount_path.clone(),
            lustre: lustre_api::LustreApi,
        });
        let concurrency = def.action.nb_threads.map(|n| n.max(1) as usize).unwrap_or(1);
        let rate_limiter = def
            .action
            .rate_limit
            .as_ref()
            .and_then(crate::ratelimit::RateLimiter::from_spec);

        let parent_cancel = ctx.cancellation_token.clone();
        let cancel = parent_cancel.child_token();

        // Low-watermark monitor: cancel dispatch when measure drops below low threshold.
        let _low_wm_guard = maybe_spawn_low_watermark_monitor(def, &where_clause, &query_params, cancel.clone(), rt);

        let candidate_count = candidates.len();
        let effective_executor: Arc<dyn rbh_actions::ActionExecutor> = if self.dry_run {
            tracing::info!(policy_id = self.policy_id, "dry_run mode");
            Arc::new(DryRunExecutor)
        } else {
            executor
        };

        let (success, skipped, failed) = dispatch_workers(
            candidates,
            effective_executor,
            action_ctx,
            concurrency,
            rate_limiter,
            cancel,
            self.policy_id,
            def.action.max_volume,
            def.action.timeout_secs,
            def.action.retry,
            def.action.skip_hardlinked,
        )
        .await;

        tracing::info!(
            policy_id = self.policy_id,
            candidates = candidate_count,
            success,
            skipped,
            failed,
            "policy run completed"
        );

        let outcome = if failed == 0 && candidate_count == 0 {
            "empty"
        } else if failed == 0 {
            "success"
        } else if success > 0 {
            "partial"
        } else {
            "failed"
        };
        rbh_observability::metrics::ACTIONS
            .with_label_values(&[pid_lbl.as_str(), "success"])
            .inc_by(success);
        rbh_observability::metrics::ACTIONS
            .with_label_values(&[pid_lbl.as_str(), "skipped"])
            .inc_by(skipped);
        rbh_observability::metrics::ACTIONS
            .with_label_values(&[pid_lbl.as_str(), "failed"])
            .inc_by(failed);
        record_run(&pid_lbl, outcome, run_started);
        Ok(())
    }
}

fn record_run(pid: &str, outcome: &str, started: std::time::Instant) {
    rbh_observability::metrics::POLICY_RUN_DURATION
        .with_label_values(&[pid])
        .observe(started.elapsed().as_secs_f64());
    rbh_observability::metrics::POLICY_RUNS
        .with_label_values(&[pid, outcome])
        .inc();
}

/// Build the action executor for a policy's kind, reading parameters from `ActionOpts`.
fn make_executor(def: &crate::PolicyDef) -> Result<Arc<dyn rbh_actions::ActionExecutor>, &'static str> {
    use crate::PolicyKind::*;
    Ok(match def.kind {
        Purge => Arc::new(rbh_actions::PurgeExecutor),
        HsmArchive => {
            let (archive_id, hints) = def
                .action
                .hsm
                .as_ref()
                .map(|h| {
                    (
                        h.archive_id.unwrap_or(1),
                        h.hints.as_ref().map(|s| s.as_bytes().to_vec()),
                    )
                })
                .unwrap_or((1, None));
            Arc::new(rbh_actions::HsmArchiveExecutor { archive_id, hints })
        }
        HsmRelease => Arc::new(rbh_actions::HsmReleaseExecutor),
        HsmRestore => Arc::new(rbh_actions::HsmRestoreExecutor),
        HsmRemove => Arc::new(rbh_actions::HsmRemoveExecutor),
        Migration => {
            let c = def.action.cmd.as_ref().ok_or("migration policy has no cmd config")?;
            Arc::new(rbh_actions::CmdExecutor::new(
                &c.command,
                c.args.clone(),
                c.timeout_secs.or(def.action.timeout_secs),
                c.cmd_vars.clone(),
            ))
        }
        Alert => {
            let (webhook, log, message) = def
                .action
                .alert
                .as_ref()
                .map(|a| (a.webhook.clone(), a.log, a.message.clone()))
                .unwrap_or((None, true, None));
            Arc::new(rbh_actions::AlertExecutor::new(webhook, log, message))
        }
        Backup => {
            let cfg = def.action.backup.as_ref().ok_or("backup policy has no backup config")?;
            let (archive_id, hints) = def
                .action
                .hsm
                .as_ref()
                .map(|h| (h.archive_id.unwrap_or(1), h.hints.clone()))
                .unwrap_or((1, None));
            Arc::new(rbh_actions::BackupExecutor::from_config(
                cfg,
                rbh_backup::BackupOp::Archive,
                archive_id,
                hints,
                cfg.dest_template.clone(),
            ))
        }
    })
}

// ── Low-watermark monitor ─────────────────────────────────────────────────────

struct MonitorGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn maybe_spawn_low_watermark_monitor(
    def: &crate::PolicyDef, where_clause: &str, query_params: &[rbh_entry_store::store::QueryParam],
    stop_token: CancellationToken, rt: &Arc<PolicyRuntime>,
) -> Option<MonitorGuard> {
    // Parse the trigger string to find low watermark.
    let spec = crate::trigger_parser::parse_trigger(&def.trigger).ok()?;
    let (interval_secs, measure) = match spec {
        crate::model::TriggerSpec::ThresholdCount {
            check_interval_secs,
            low_count,
            ..
        } if low_count > 0 => (check_interval_secs, LowMeasure::Count(low_count)),
        crate::model::TriggerSpec::ThresholdVolume {
            check_interval_secs,
            low_bytes,
            ..
        } if low_bytes > 0 => (check_interval_secs, LowMeasure::Volume(low_bytes)),
        _ => return None,
    };

    let entry_store = rt.entry_store.clone();
    let where_clause = where_clause.to_string();
    let params: Vec<_> = query_params.to_vec();
    let policy_name = def.name.clone();
    let interval = std::time::Duration::from_secs(interval_secs.max(1));

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_token.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            let hit = match measure {
                LowMeasure::Count(low) => entry_store
                    .count_where(&where_clause, &params)
                    .await
                    .map(|c| c <= low)
                    .unwrap_or(false),
                LowMeasure::Volume(low) => entry_store
                    .sum_size_where(&where_clause, &params)
                    .await
                    .map(|v| v <= low)
                    .unwrap_or(false),
            };
            if hit {
                tracing::info!(policy = %policy_name, "low watermark reached — stopping policy run");
                stop_token.cancel();
                return;
            }
        }
    });
    Some(MonitorGuard { handle })
}

#[derive(Debug, Clone, Copy)]
enum LowMeasure {
    Count(u64),
    Volume(u64),
}

// ── Dry-run executor ──────────────────────────────────────────────────────────

struct DryRunExecutor;

#[async_trait]
impl rbh_actions::ActionExecutor for DryRunExecutor {
    async fn execute(
        &self, entry: &rbh_entry_store::model::EntryRow, _ctx: &rbh_actions::ActionContext,
    ) -> Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
        tracing::info!(fid = %entry.fid, size = entry.size, "dry_run: would act on entry");
        Ok(rbh_actions::ActionOutcome::Skipped {
            reason: "dry run".into(),
        })
    }
}

// ── Dispatch workers ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn dispatch_workers(
    candidates: Vec<rbh_entry_store::model::EntryRow>, executor: Arc<dyn rbh_actions::ActionExecutor>,
    action_ctx: Arc<rbh_actions::ActionContext>, concurrency: usize,
    rate_limiter: Option<crate::ratelimit::RateLimiter>, cancel: CancellationToken, policy_id: u64,
    max_volume: Option<u64>, timeout_secs: Option<u64>, retry: Option<crate::model::RetryParams>,
    skip_hardlinked: bool,
) -> (u64, u64, u64) {
    use tokio::sync::Semaphore;

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();
    let mut bytes_dispatched: u64 = 0;
    let mut hardlinked_skipped: u64 = 0;

    for entry in candidates {
        if cancel.is_cancelled() {
            tracing::info!(policy_id, "cancelled before full dispatch");
            break;
        }
        if skip_hardlinked && entry.nlink > 1 {
            hardlinked_skipped += 1;
            continue;
        }
        if let Some(cap) = max_volume {
            if bytes_dispatched.saturating_add(entry.size) > cap {
                tracing::info!(policy_id, "max_volume reached — stopping dispatch");
                break;
            }
            bytes_dispatched = bytes_dispatched.saturating_add(entry.size);
        }

        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        if let Some(ref rl) = rate_limiter {
            rl.acquire(entry.size).await;
        }
        let exec = executor.clone();
        let ctx = action_ctx.clone();
        let worker_cancel = cancel.clone();
        let max_attempts = retry.map(|r| r.max_attempts.max(1)).unwrap_or(1);
        let backoff = retry
            .map(|r| std::time::Duration::from_secs(r.backoff_secs))
            .unwrap_or_default();

        set.spawn(async move {
            if worker_cancel.is_cancelled() {
                drop(permit);
                return WorkerOutcome::Skipped;
            }
            let fid = entry.fid;
            let mut last: Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> =
                Ok(rbh_actions::ActionOutcome::Failed {
                    error: "no attempts".into(),
                });

            for attempt in 1..=max_attempts {
                if worker_cancel.is_cancelled() {
                    break;
                }
                let fut = exec.execute(&entry, &ctx);
                let result = match timeout_secs {
                    Some(secs) => match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
                        Ok(r) => r,
                        Err(_) => Ok(rbh_actions::ActionOutcome::Failed {
                            error: format!("timeout after {secs}s"),
                        }),
                    },
                    None => fut.await,
                };
                let terminal = matches!(
                    &result,
                    Ok(rbh_actions::ActionOutcome::Success) | Ok(rbh_actions::ActionOutcome::Skipped { .. })
                );
                last = result;
                if terminal || attempt == max_attempts {
                    break;
                }
                tokio::time::sleep(backoff).await;
            }
            drop(permit);
            match last {
                Ok(rbh_actions::ActionOutcome::Success) => {
                    tracing::debug!(%fid, "action success");
                    WorkerOutcome::Success
                }
                Ok(rbh_actions::ActionOutcome::Skipped { reason }) => {
                    tracing::debug!(%fid, %reason, "action skipped");
                    WorkerOutcome::Skipped
                }
                Ok(rbh_actions::ActionOutcome::Failed { error }) => {
                    tracing::warn!(%fid, %error, "action failed");
                    WorkerOutcome::Failed
                }
                Err(e) => {
                    tracing::warn!(%fid, error = %e, "action error");
                    WorkerOutcome::Failed
                }
            }
        });
    }

    let mut success = 0u64;
    let mut skipped = hardlinked_skipped;
    let mut failed = 0u64;
    while let Some(j) = set.join_next().await {
        match j {
            Ok(WorkerOutcome::Success) => success += 1,
            Ok(WorkerOutcome::Skipped) => skipped += 1,
            Ok(WorkerOutcome::Failed) | Err(_) => failed += 1,
        }
    }
    if hardlinked_skipped > 0 {
        tracing::info!(policy_id, hardlinked_skipped, "skipped hardlinked entries");
    }
    (success, skipped, failed)
}

#[derive(Debug)]
enum WorkerOutcome {
    Success,
    Skipped,
    Failed,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use lustre_api::LuFid;
    use rbh_entry_store::model::{EntryKind, EntryRow};

    fn test_entry() -> EntryRow {
        EntryRow {
            fid: LuFid::new(0x200000401, 0x42, 0),
            parent_fid: Some(LuFid::new(0x200000401, 0x01, 0)),
            name: Bytes::from_static(b"report.csv"),
            kind: EntryKind::File,
            size: 5_000_000,
            blocks: 4096,
            uid: 1000,
            gid: 100,
            projid: 0,
            mode: 0o644,
            nlink: 1,
            atime: 1_775_955_820,
            mtime: 1_600_000_000,
            ctime: 1_600_000_000,
            stripe_count: Some(2),
            stripe_size: Some(4_194_304),
            pool_name: Some("ssd".to_string()),
            sm_status: serde_json::json!({}),
            last_seen: 1_775_955_820,
            depth: 2,
        }
    }

    #[test]
    fn target_filter_fs_is_true() {
        assert_eq!(TargetFilter::Fs.to_predicate(), rbh_predicate::Predicate::True);
    }

    #[test]
    fn target_filter_ost_produces_on_ost() {
        let p = TargetFilter::Ost { osts: vec![2, 5] }.to_predicate();
        assert_eq!(p, rbh_predicate::Predicate::OnOst { osts: vec![2, 5] });
    }

    #[test]
    fn target_filter_pool_produces_in_pool() {
        let p = TargetFilter::Pool { name: "flash".into() }.to_predicate();
        assert_eq!(p, rbh_predicate::Predicate::InPool { pool: "flash".into() });
    }

    #[test]
    fn target_filter_serde_default_is_fs() {
        let json = r#"{"policy_id":1}"#;
        let t: PolicyRunTask = serde_json::from_str(json).unwrap();
        assert_eq!(t.target, TargetFilter::Fs);
    }

    #[test]
    fn target_filter_serde_ost_roundtrip() {
        let t = PolicyRunTask {
            policy_id: 1,
            trigger_idx: 0,
            target: TargetFilter::Ost { osts: vec![7] },
            dry_run: false,
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: PolicyRunTask = serde_json::from_str(&s).unwrap();
        assert_eq!(back.target, TargetFilter::Ost { osts: vec![7] });
    }

    #[test]
    fn low_measure_decodes_from_trigger_spec() {
        // parse_trigger("count > 500") → ThresholdCount with high=500
        let spec = crate::trigger_parser::parse_trigger("count > 500").unwrap();
        assert!(matches!(
            spec,
            crate::model::TriggerSpec::ThresholdCount { high_count: 500, .. }
        ));
    }

    // --- dispatch_workers tests -----------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct RecordingExecutor {
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        outcome: rbh_actions::ActionOutcome,
        delay: Duration,
    }

    #[async_trait]
    impl rbh_actions::ActionExecutor for RecordingExecutor {
        async fn execute(
            &self, _entry: &EntryRow, _ctx: &rbh_actions::ActionContext,
        ) -> Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(self.outcome.clone())
        }
    }

    fn mk_candidates(n: usize) -> Vec<EntryRow> {
        (0..n)
            .map(|i| {
                let mut e = test_entry();
                e.fid = LuFid::new(0x200000401, i as u32 + 100, 0);
                e
            })
            .collect()
    }

    fn default_dispatch(
        candidates: Vec<EntryRow>, exec: Arc<dyn rbh_actions::ActionExecutor>, concurrency: usize,
        cancel: CancellationToken,
    ) -> impl std::future::Future<Output = (u64, u64, u64)> {
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        dispatch_workers(
            candidates,
            exec,
            ctx,
            concurrency,
            None,
            cancel,
            0,
            None,
            None,
            None,
            false,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_respects_concurrency_cap() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: in_flight.clone(),
            max_in_flight: max_in_flight.clone(),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(30),
        });
        let cancel = CancellationToken::new();
        let (succ, _, _) = default_dispatch(mk_candidates(12), exec, 3, cancel).await;
        assert_eq!(succ, 12);
        let peak = max_in_flight.load(Ordering::SeqCst);
        assert!((2..=3).contains(&peak), "expected peak ~3, got {peak}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_skips_hardlinked_when_opted_in() {
        let mut cands = mk_candidates(10);
        for (i, e) in cands.iter_mut().enumerate() {
            e.nlink = if i % 2 == 0 { 1 } else { 2 };
        }
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::ZERO,
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = CancellationToken::new();
        let (succ, skipped, _) = dispatch_workers(cands, exec, ctx, 2, None, cancel, 0, None, None, None, true).await;
        assert_eq!(succ, 5);
        assert!(skipped >= 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_stops_on_cancel() {
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(10),
        });
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancel
        let (succ, _, _) = default_dispatch(mk_candidates(100), exec, 4, cancel).await;
        assert!(succ < 100, "expected partial completion after pre-cancel");
    }

    struct FailThenSucceedExecutor {
        attempt: Arc<AtomicUsize>,
        fail_until: usize,
    }

    #[async_trait]
    impl rbh_actions::ActionExecutor for FailThenSucceedExecutor {
        async fn execute(
            &self, _e: &EntryRow, _ctx: &rbh_actions::ActionContext,
        ) -> Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
            let n = self.attempt.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_until {
                Ok(rbh_actions::ActionOutcome::Failed {
                    error: "simulated".into(),
                })
            } else {
                Ok(rbh_actions::ActionOutcome::Success)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_retries_until_success() {
        let attempt = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(FailThenSucceedExecutor {
            attempt: attempt.clone(),
            fail_until: 2,
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let retry = Some(crate::model::RetryParams {
            max_attempts: 3,
            backoff_secs: 0,
        });
        let cancel = CancellationToken::new();
        let (succ, _, failed) = dispatch_workers(
            mk_candidates(1),
            exec,
            ctx,
            1,
            None,
            cancel,
            0,
            None,
            None,
            retry,
            false,
        )
        .await;
        assert_eq!(succ, 1, "should succeed on 3rd attempt");
        assert_eq!(failed, 0);
        assert_eq!(attempt.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_retry_exhausts_then_fails() {
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(FailThenSucceedExecutor {
            attempt: Arc::new(AtomicUsize::new(0)),
            fail_until: 10, // more than max_attempts
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let retry = Some(crate::model::RetryParams {
            max_attempts: 2,
            backoff_secs: 0,
        });
        let cancel = CancellationToken::new();
        let (succ, _, failed) = dispatch_workers(
            mk_candidates(1),
            exec,
            ctx,
            1,
            None,
            cancel,
            0,
            None,
            None,
            retry,
            false,
        )
        .await;
        assert_eq!(succ, 0);
        assert_eq!(failed, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_respects_max_volume() {
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::ZERO,
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        // Each test_entry has size=5_000_000; cap at 12_000_000 → 2 entries max.
        let cancel = CancellationToken::new();
        let (succ, _, _) = dispatch_workers(
            mk_candidates(10),
            exec,
            ctx,
            1,
            None,
            cancel,
            0,
            Some(12_000_000),
            None,
            None,
            false,
        )
        .await;
        assert!(succ <= 2, "expected <=2 entries within volume cap, got {succ}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_timeout_marks_failed() {
        struct SlowExecutor;
        #[async_trait]
        impl rbh_actions::ActionExecutor for SlowExecutor {
            async fn execute(
                &self, _e: &EntryRow, _ctx: &rbh_actions::ActionContext,
            ) -> Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(rbh_actions::ActionOutcome::Success)
            }
        }
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = CancellationToken::new();
        let (succ, _, failed) = dispatch_workers(
            mk_candidates(1),
            Arc::new(SlowExecutor),
            ctx,
            1,
            None,
            cancel,
            0,
            None,
            Some(1), // 1-second timeout
            None,
            false,
        )
        .await;
        assert_eq!(succ, 0);
        assert_eq!(failed, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_honors_rate_limit() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: in_flight.clone(),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(1),
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = CancellationToken::new();
        let rl = crate::ratelimit::RateLimiter::from_spec(&crate::model::RateLimit {
            max_per_sec: Some(10),
            max_bytes_per_sec: None,
        });
        let (succ, _, _) =
            dispatch_workers(mk_candidates(30), exec, ctx, 4, rl, cancel, 0, None, None, None, false).await;
        assert_eq!(succ, 30);
    }
}
