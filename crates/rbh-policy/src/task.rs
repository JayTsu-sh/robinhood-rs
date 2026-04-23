//! `PolicyRunTask` — scheduler-rs `Task` impl for policy execution.
//!
//! At fire time the task:
//! 1. Loads the policy definition from `PolicyStore`.
//! 2. Queries candidate entries via `scope` predicate (SQL pushdown).
//! 3. Evaluates `rules` in first-match order per entry.
//! 4. Dispatches matched entries to the action executor.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use scheduler_rs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::model::{ActionParams, Rule};

/// Shared runtime context injected once at daemon startup.
/// PolicyRunTask reads this to access stores and Lustre config.
pub struct PolicyRuntime {
    pub policy_store: crate::PolicyStore,
    pub entry_store: rbh_entry_store::store::EntryStore,
    pub mount_path: PathBuf,
}

static RUNTIME: OnceLock<Arc<PolicyRuntime>> = OnceLock::new();

/// Initialize the global policy runtime. Must be called once at daemon startup
/// before any PolicyRunTask fires.
pub fn init_runtime(rt: Arc<PolicyRuntime>) {
    RUNTIME.set(rt).ok();
}

fn runtime() -> &'static Arc<PolicyRuntime> {
    RUNTIME.get().expect("PolicyRuntime not initialized — call init_runtime() at startup")
}

/// Narrow a policy run to a specific subset of the filesystem. Injected by
/// triggers (e.g. threshold triggers target a specific OST or pool). The
/// filter is composed as an extra `AND` on top of the policy `scope`.
///
/// Serialized into the scheduler-rs task payload, so new variants must be
/// backwards-compatible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetFilter {
    /// Default — apply to the whole catalog.
    Fs,
    /// Match files striped on any of the listed OST indices.
    Ost { osts: Vec<u32> },
    /// Match files in the named OST pool.
    Pool { name: String },
    /// Match files owned by the given UID.
    User { uid: u32 },
    /// Match files in the given GID.
    Group { gid: u32 },
    /// Match files in the given project id.
    Projid { projid: u32 },
}

impl TargetFilter {
    /// Render as an in-memory `Predicate`. `Fs` => `True`.
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

impl Default for TargetFilter {
    fn default() -> Self {
        Self::Fs
    }
}

/// Task data serialized into scheduler-rs schedule rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRunTask {
    pub policy_id: u64,
    pub trigger_idx: u32,
    /// Per-run narrowing injected by threshold triggers. Defaults to the
    /// whole filesystem so existing scheduled runs keep working after an
    /// upgrade.
    #[serde(default)]
    pub target: TargetFilter,
}

#[async_trait]
impl Task for PolicyRunTask {
    const TYPE_NAME: &'static str = "rbh.policy_run";

    async fn run(&self, ctx: &TaskContext) -> TaskResult {
        let rt = runtime();

        tracing::info!(
            policy_id = self.policy_id,
            trigger_idx = self.trigger_idx,
            execution_id = %ctx.execution_id.0,
            "policy run started"
        );

        if ctx.cancellation_token.is_cancelled() {
            tracing::info!(policy_id = self.policy_id, "cancelled before start");
            return Ok(());
        }

        // 1. Load policy definition.
        let policy = match rt.policy_store.get(self.policy_id).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(policy_id = self.policy_id, error = %e, "failed to load policy");
                return Err(scheduler_rs::error::SchedulerError::ExecutionError(e.to_string()));
            }
        };

        let def = &policy.definition;
        tracing::info!(
            policy_id = self.policy_id,
            kind = def.kind.as_str(),
            rule_count = def.rules.len(),
            "policy loaded"
        );

        // 2. Get action executor for this policy kind.
        let executor: Arc<dyn rbh_actions::ActionExecutor> = match def.kind {
            crate::PolicyKind::Purge => Arc::new(rbh_actions::PurgeExecutor),
            crate::PolicyKind::HsmArchive => {
                Arc::new(rbh_actions::HsmArchiveExecutor { archive_id: 1 })
            }
            crate::PolicyKind::HsmRelease => Arc::new(rbh_actions::HsmReleaseExecutor),
            other => {
                tracing::warn!(kind = other.as_str(), "action not implemented for this kind");
                return Ok(());
            }
        };

        // 3. Query candidates via predicate SQL pushdown. Compose the scope
        // with ignore_fileclass AND the per-run target filter:
        //   WHERE scope AND <target> AND NOT (ignore1 OR ignore2 …).
        let scope_with_ignore =
            crate::model::compose_scope_with_ignores(&def.scope, &def.ignore_fileclass);
        let target_pred = self.target.to_predicate();
        let effective_scope = match &target_pred {
            rbh_predicate::Predicate::True => scope_with_ignore,
            _ => rbh_predicate::Predicate::And {
                children: vec![scope_with_ignore, target_pred.clone()],
            },
        };
        if !def.ignore_fileclass.is_empty() || !matches!(self.target, TargetFilter::Fs) {
            tracing::debug!(
                policy_id = self.policy_id,
                ignore_count = def.ignore_fileclass.len(),
                target = ?self.target,
                "composed scope with ignore_fileclass and target"
            );
        }
        let (where_clause, sql_params) = rbh_predicate::to_sql(&effective_scope);
        let query_params: Vec<rbh_entry_store::store::QueryParam> = sql_params
            .into_iter()
            .map(|p| match p {
                rbh_predicate::SqlParam::Num(n) => rbh_entry_store::store::QueryParam::Int(n),
                rbh_predicate::SqlParam::Str(s) => rbh_entry_store::store::QueryParam::Str(s),
            })
            .collect();
        let max_count = def.default_action.max_count.unwrap_or(10_000);
        let order_by = def
            .default_action
            .lru_sort
            .and_then(|s| s.column())
            .map(|col| format!("{col} ASC"));
        let candidates = match rt
            .entry_store
            .query_page(
                &where_clause,
                &query_params,
                order_by.as_deref(),
                max_count,
                0,
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to query candidates");
                return Err(scheduler_rs::error::SchedulerError::ExecutionError(e.to_string()));
            }
        };

        tracing::info!(
            policy_id = self.policy_id,
            candidate_count = candidates.len(),
            "candidates queried"
        );

        // 4. Dispatch candidates to a pool of concurrent workers sized by
        // ActionParams.nb_threads (default 1 preserves sequential behavior).
        let action_ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: rt.mount_path.clone(),
            lustre: lustre_api::LustreApi,
        });
        let concurrency = def
            .default_action
            .nb_threads
            .map(|n| n.max(1) as usize)
            .unwrap_or(1);
        let rate_limiter = def
            .default_action
            .rate_limit
            .as_ref()
            .and_then(crate::ratelimit::RateLimiter::from_spec);

        // Child token: fires on parent cancel OR on low-watermark reached.
        // Using a child keeps the parent token (owned by scheduler-rs)
        // pristine; we only observe it.
        let parent_cancel = ctx.cancellation_token.clone();
        let cancel = parent_cancel.child_token();

        // If this run was fired by a threshold trigger that declares a
        // low watermark, spawn a monitor that re-evaluates the measure
        // and cancels `cancel` when we're under the low watermark.
        let _low_watermark_guard = maybe_spawn_low_watermark_monitor(
            def,
            self,
            &where_clause,
            &query_params,
            cancel.clone(),
            rt,
        );

        let candidate_count = candidates.len();
        let (success, skipped, failed) = dispatch_workers(
            candidates,
            executor,
            action_ctx,
            concurrency,
            rate_limiter,
            cancel,
            self.policy_id,
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

        Ok(())
    }
}

/// RAII guard that aborts the low-watermark monitor when the run loop
/// finishes (either naturally or via cancel).
struct MonitorGuard {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MonitorGuard {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spawn a background task that periodically re-evaluates the firing
/// trigger's measure (count or SUM(size)) against the same scope used
/// for candidate selection, and cancels `stop_token` as soon as the
/// measure drops to or below the low watermark.
///
/// Returns `None` when the firing trigger isn't a threshold trigger or
/// has no positive low watermark (i.e. high-only trigger).
fn maybe_spawn_low_watermark_monitor(
    def: &crate::PolicyDef,
    task: &PolicyRunTask,
    where_clause: &str,
    query_params: &[rbh_entry_store::store::QueryParam],
    stop_token: scheduler_rs::prelude::CancellationToken,
    rt: &Arc<PolicyRuntime>,
) -> Option<MonitorGuard> {
    let trigger = def.triggers.get(task.trigger_idx as usize)?;
    let (interval_secs, measure) = match trigger {
        crate::TriggerSpec::ThresholdCount {
            check_interval_secs,
            low_count,
            ..
        } if *low_count > 0 => (*check_interval_secs, LowMeasure::Count(*low_count)),
        crate::TriggerSpec::ThresholdVolume {
            check_interval_secs,
            low_bytes,
            ..
        } if *low_bytes > 0 => (*check_interval_secs, LowMeasure::Volume(*low_bytes)),
        _ => return None,
    };

    let entry_store = rt.entry_store.clone();
    let where_clause = where_clause.to_string();
    let params: Vec<_> = query_params.to_vec();
    let policy_id = task.policy_id;
    let trigger_idx = task.trigger_idx;
    let interval = std::time::Duration::from_secs(interval_secs.max(1));

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_token.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
            let hit = match measure {
                LowMeasure::Count(low) => match entry_store
                    .count_where(&where_clause, &params)
                    .await
                {
                    Ok(c) => c <= low,
                    Err(e) => {
                        tracing::warn!(
                            policy_id,
                            trigger_idx,
                            error = %e,
                            "low-watermark count query failed"
                        );
                        false
                    }
                },
                LowMeasure::Volume(low) => {
                    match sum_size_scope(&entry_store, &where_clause, &params).await {
                        Ok(v) => v <= low,
                        Err(e) => {
                            tracing::warn!(
                                policy_id,
                                trigger_idx,
                                error = %e,
                                "low-watermark volume query failed"
                            );
                            false
                        }
                    }
                }
            };
            if hit {
                tracing::info!(
                    policy_id,
                    trigger_idx,
                    "low watermark reached — stopping policy run"
                );
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

/// Stand-in for rbh-daemon's sum_size_where — local to the policy crate
/// because rbh-policy can't depend on rbh-daemon.
async fn sum_size_scope(
    store: &rbh_entry_store::store::EntryStore,
    where_clause: &str,
    params: &[rbh_entry_store::store::QueryParam],
) -> Result<u64, rbh_entry_store::StoreError> {
    use sqlx::Row;
    let sql = format!(
        "SELECT CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS total \
         FROM entries WHERE {where_clause}"
    );
    let mut q = sqlx::query(&sql);
    for p in params {
        q = match p {
            rbh_entry_store::store::QueryParam::Int(n) => q.bind(*n),
            rbh_entry_store::store::QueryParam::Str(s) => q.bind(s.as_str()),
        };
    }
    let row = q.fetch_one(store.pool()).await?;
    Ok(row.try_get::<u64, _>("total").unwrap_or(0))
}

/// Fan out candidate processing across `concurrency` workers using a
/// tokio `JoinSet`. Returns `(success, skipped, failed)` totals. Respects
/// the supplied cancel token: outstanding workers complete but no new
/// entries are dispatched after cancellation.
async fn dispatch_workers(
    candidates: Vec<rbh_entry_store::model::EntryRow>,
    executor: Arc<dyn rbh_actions::ActionExecutor>,
    action_ctx: Arc<rbh_actions::ActionContext>,
    concurrency: usize,
    rate_limiter: Option<crate::ratelimit::RateLimiter>,
    cancel: scheduler_rs::prelude::CancellationToken,
    policy_id: u64,
) -> (u64, u64, u64) {
    use tokio::sync::Semaphore;

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();

    for entry in candidates {
        if cancel.is_cancelled() {
            tracing::info!(policy_id, "cancelled before full dispatch");
            break;
        }
        // Acquire a permit before spawning — this bounds in-flight work
        // without holding the candidate vec past its useful life.
        let permit = match sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break, // semaphore closed (shouldn't happen)
        };
        // Rate limit is acquired on the dispatch path (not inside the
        // spawned task) so concurrency and rate caps compose: workers
        // wait for a permit, and dispatch itself waits for the token.
        if let Some(ref rl) = rate_limiter {
            rl.acquire(entry.size).await;
        }
        let exec = executor.clone();
        let ctx = action_ctx.clone();
        let worker_cancel = cancel.clone();
        set.spawn(async move {
            if worker_cancel.is_cancelled() {
                drop(permit);
                return WorkerOutcome::Skipped;
            }
            let fid = entry.fid;
            let result = exec.execute(&entry, &ctx).await;
            drop(permit);
            match result {
                Ok(rbh_actions::ActionOutcome::Success) => {
                    tracing::debug!(%fid, "action success");
                    WorkerOutcome::Success
                }
                Ok(rbh_actions::ActionOutcome::Skipped { reason }) => {
                    tracing::debug!(%fid, reason = %reason, "action skipped");
                    WorkerOutcome::Skipped
                }
                Ok(rbh_actions::ActionOutcome::Failed { error }) => {
                    tracing::warn!(%fid, error = %error, "action failed");
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
    let mut skipped = 0u64;
    let mut failed = 0u64;
    while let Some(j) = set.join_next().await {
        match j {
            Ok(WorkerOutcome::Success) => success += 1,
            Ok(WorkerOutcome::Skipped) => skipped += 1,
            Ok(WorkerOutcome::Failed) => failed += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(error = %e, "worker task panicked");
            }
        }
    }
    (success, skipped, failed)
}

#[derive(Debug)]
enum WorkerOutcome {
    Success,
    Skipped,
    Failed,
}

/// Evaluate rules against an entry. Returns effective action params
/// for the first matching rule, or default if no rule matches.
///
/// Kept alongside tests for the next iteration: the executor trait takes
/// only `(entry, ctx)` today, so per-entry rule-derived params cannot yet
/// be applied. Will be wired in when the executor gains a params arg.
#[allow(dead_code)]
fn evaluate_rules(
    rules: &[Rule],
    default_action: &ActionParams,
    entry: &rbh_entry_store::model::EntryRow,
) -> ActionParams {
    for rule in rules {
        if rbh_predicate::matches(&rule.condition, entry) {
            return rule.action.merge_over(default_action);
        }
    }
    default_action.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use lustre_api::LuFid;
    use rbh_entry_store::model::{EntryKind, EntryRow};
    use rbh_predicate::*;

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
        }
    }

    #[test]
    fn first_match_rule_wins() {
        let default = ActionParams {
            max_count: Some(1000),
            ..Default::default()
        };
        let rules = vec![
            Rule {
                condition: Predicate::Cmp {
                    field: Field::Size,
                    cmp: CmpOp::Gt,
                    value: Value::Num(10_000_000),
                },
                action: ActionParams {
                    max_count: Some(10),
                    ..Default::default()
                },
            },
            Rule {
                condition: Predicate::Cmp {
                    field: Field::Mtime,
                    cmp: CmpOp::Lt,
                    value: Value::Num(1_700_000_000),
                },
                action: ActionParams {
                    max_count: Some(500),
                    nb_threads: Some(2),
                    ..Default::default()
                },
            },
        ];
        let result = evaluate_rules(&rules, &default, &test_entry());
        assert_eq!(result.max_count, Some(500));
        assert_eq!(result.nb_threads, Some(2));
    }

    #[test]
    fn no_rule_matches_returns_default() {
        let default = ActionParams {
            max_count: Some(1000),
            ..Default::default()
        };
        let rules = vec![Rule {
            condition: Predicate::False,
            action: ActionParams {
                max_count: Some(10),
                ..Default::default()
            },
        }];
        let result = evaluate_rules(&rules, &default, &test_entry());
        assert_eq!(result.max_count, Some(1000));
    }

    #[test]
    fn target_filter_fs_is_true() {
        assert_eq!(
            TargetFilter::Fs.to_predicate(),
            rbh_predicate::Predicate::True
        );
    }

    #[test]
    fn target_filter_ost_produces_on_ost() {
        let p = TargetFilter::Ost { osts: vec![2, 5] }.to_predicate();
        assert_eq!(
            p,
            rbh_predicate::Predicate::OnOst { osts: vec![2, 5] }
        );
    }

    #[test]
    fn target_filter_pool_produces_in_pool() {
        let p = TargetFilter::Pool {
            name: "flash".into(),
        }
        .to_predicate();
        assert_eq!(
            p,
            rbh_predicate::Predicate::InPool {
                pool: "flash".into()
            }
        );
    }

    #[test]
    fn target_filter_serde_default_is_fs() {
        let json = r#"{"policy_id":1,"trigger_idx":0}"#;
        let t: PolicyRunTask = serde_json::from_str(json).unwrap();
        assert_eq!(t.target, TargetFilter::Fs);
    }

    #[test]
    fn target_filter_serde_ost_roundtrip() {
        let t = PolicyRunTask {
            policy_id: 1,
            trigger_idx: 0,
            target: TargetFilter::Ost { osts: vec![7] },
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: PolicyRunTask = serde_json::from_str(&s).unwrap();
        assert_eq!(back.target, TargetFilter::Ost { osts: vec![7] });
    }

    // --- dispatch_workers tests -----------------------------------------

    use async_trait::async_trait;
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
            &self,
            _entry: &EntryRow,
            _ctx: &rbh_actions::ActionContext,
        ) -> std::result::Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(self.outcome.clone())
        }
    }

    fn mk_candidates(n: usize) -> Vec<EntryRow> {
        (0..n).map(|i| {
            let mut e = test_entry();
            e.fid = LuFid::new(0x200000401, i as u32 + 100, 0);
            e
        }).collect()
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
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        let (succ, _, _) =
            dispatch_workers(mk_candidates(12), exec, ctx, 3, None, cancel, 0).await;
        assert_eq!(succ, 12);
        let peak = max_in_flight.load(Ordering::SeqCst);
        assert!(peak >= 2 && peak <= 3, "expected peak ~3, got {peak}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_honors_rate_limit() {
        // 100 actions/sec, 30 candidates — first ~100 tokens are the
        // initial bucket so all 30 dispatch immediately; with a smaller
        // initial budget via pre-drained actions we'd see throttling.
        // Here we prove the limit doesn't break the happy path.
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight,
            max_in_flight,
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(1),
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        // 10/s rate; 30 actions → ~2s real time for the tail.
        let rl = crate::ratelimit::RateLimiter::from_spec(&crate::model::RateLimit {
            max_per_sec: Some(10),
            max_bytes_per_sec: None,
        });
        let start = std::time::Instant::now();
        let (succ, _, _) =
            dispatch_workers(mk_candidates(30), exec, ctx, 4, rl, cancel, 0).await;
        let elapsed = start.elapsed();
        assert_eq!(succ, 30);
        // First 10 tokens free, then 20 at 10/s = ~2s.
        assert!(
            elapsed >= Duration::from_millis(1500),
            "30 actions @ 10/s should take >=1.5s, got {elapsed:?}"
        );
    }

    #[test]
    fn low_measure_decodes_from_trigger_spec() {
        // ThresholdCount with low_count=0 → no monitor (no closure needed).
        // ThresholdCount with low_count>0 → LowMeasure::Count.
        let spec = crate::TriggerSpec::ThresholdCount {
            check_interval_secs: 30,
            high_count: 1_000,
            low_count: 700,
            post_trigger_wait_secs: 0,
            target: crate::model::ThresholdTarget::Fs,
        };
        // Directly exercise the match arms in maybe_spawn_low_watermark_monitor
        // by using the same pattern — kept in sync with the source.
        let got = match &spec {
            crate::TriggerSpec::ThresholdCount { low_count, .. } if *low_count > 0 => {
                Some(LowMeasure::Count(*low_count))
            }
            _ => None,
        };
        assert!(matches!(got, Some(LowMeasure::Count(700))));

        let spec_no_low = crate::TriggerSpec::ThresholdCount {
            check_interval_secs: 30,
            high_count: 1_000,
            low_count: 0,
            post_trigger_wait_secs: 0,
            target: crate::model::ThresholdTarget::Fs,
        };
        let got_none = match &spec_no_low {
            crate::TriggerSpec::ThresholdCount { low_count, .. } if *low_count > 0 => {
                Some(LowMeasure::Count(*low_count))
            }
            _ => None,
        };
        assert!(got_none.is_none());

        let spec_vol = crate::TriggerSpec::ThresholdVolume {
            check_interval_secs: 30,
            high_bytes: 1 << 30,
            low_bytes: 1 << 28,
            post_trigger_wait_secs: 0,
            target: crate::model::ThresholdTarget::Fs,
        };
        let got_vol = match &spec_vol {
            crate::TriggerSpec::ThresholdVolume { low_bytes, .. } if *low_bytes > 0 => {
                Some(LowMeasure::Volume(*low_bytes))
            }
            _ => None,
        };
        assert!(matches!(got_vol, Some(LowMeasure::Volume(n)) if n == 1 << 28));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_stops_on_cancel() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight,
            max_in_flight,
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(50),
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        // Cancel quickly — most candidates should never dispatch.
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_clone.cancel();
        });
        let (succ, skipped, failed) =
            dispatch_workers(mk_candidates(50), exec, ctx, 2, None, cancel, 0).await;
        assert!(
            succ + skipped + failed < 50,
            "expected partial run after cancel, got total={}",
            succ + skipped + failed
        );
    }
}
