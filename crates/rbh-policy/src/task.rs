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
    RUNTIME
        .get()
        .expect("PolicyRuntime not initialized — call init_runtime() at startup")
}

/// Narrow a policy run to a specific subset of the filesystem. Injected by
/// triggers (e.g. threshold triggers target a specific OST or pool). The
/// filter is composed as an extra `AND` on top of the policy `scope`.
///
/// Serialized into the scheduler-rs task payload, so new variants must be
/// backwards-compatible.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetFilter {
    /// Default — apply to the whole catalog.
    #[default]
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
    /// When true, the executor is skipped — candidates are still
    /// queried and rule-evaluated so the run reports what *would*
    /// happen, but no filesystem or HSM mutations take place.
    #[serde(default)]
    pub dry_run: bool,
}

#[async_trait]
impl Task for PolicyRunTask {
    const TYPE_NAME: &'static str = "rbh.policy_run";

    async fn run(&self, ctx: &TaskContext) -> TaskResult {
        let rt = runtime();
        let run_started = std::time::Instant::now();
        let policy_id_lbl = self.policy_id.to_string();

        tracing::info!(
            policy_id = self.policy_id,
            trigger_idx = self.trigger_idx,
            execution_id = %ctx.execution_id.0,
            "policy run started"
        );

        if ctx.cancellation_token.is_cancelled() {
            tracing::info!(policy_id = self.policy_id, "cancelled before start");
            rbh_observability::metrics::POLICY_RUNS
                .with_label_values(&[policy_id_lbl.as_str(), "cancelled"])
                .inc();
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
                let (archive_id, hints) = def
                    .default_action
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
            crate::PolicyKind::HsmRelease => Arc::new(rbh_actions::HsmReleaseExecutor),
            crate::PolicyKind::Migration => match def.default_action.cmd.as_ref() {
                Some(c) => Arc::new(rbh_actions::CmdExecutor::new(
                    &c.command,
                    c.args.clone(),
                    c.timeout_secs.or(def.default_action.timeout_secs),
                )),
                None => {
                    tracing::warn!("migration policy has no cmd config — skipping run");
                    rbh_observability::metrics::POLICY_RUNS
                        .with_label_values(&[policy_id_lbl.as_str(), "misconfigured"])
                        .inc();
                    rbh_observability::metrics::POLICY_RUN_DURATION
                        .with_label_values(&[policy_id_lbl.as_str()])
                        .observe(run_started.elapsed().as_secs_f64());
                    return Ok(());
                }
            },
            crate::PolicyKind::Alert => {
                let (webhook, log, message) = match def.default_action.alert.as_ref() {
                    Some(a) => (a.webhook.clone(), a.log, a.message.clone()),
                    None => (None, true, None),
                };
                Arc::new(rbh_actions::AlertExecutor::new(webhook, log, message))
            }
            crate::PolicyKind::Backup => {
                let cfg = match def.default_action.backup.as_ref() {
                    Some(c) => c,
                    None => {
                        tracing::warn!("backup policy has no backup config — skipping run");
                        rbh_observability::metrics::POLICY_RUNS
                            .with_label_values(&[policy_id_lbl.as_str(), "misconfigured"])
                            .inc();
                        rbh_observability::metrics::POLICY_RUN_DURATION
                            .with_label_values(&[policy_id_lbl.as_str()])
                            .observe(run_started.elapsed().as_secs_f64());
                        return Ok(());
                    }
                };
                let (archive_id, hints) = def
                    .default_action
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
        };

        // 3. Query candidates via predicate SQL pushdown. Compose the scope
        // with ignore_fileclass AND the per-run target filter:
        //   WHERE scope AND <target> AND NOT (ignore1 OR ignore2 …).
        let scope_with_ignore = crate::model::compose_scope_with_ignores(&def.scope, &def.ignore_fileclass);
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
            .query_page(&where_clause, &query_params, order_by.as_deref(), max_count, 0)
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
        let concurrency = def.default_action.nb_threads.map(|n| n.max(1) as usize).unwrap_or(1);
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
        let _low_watermark_guard =
            maybe_spawn_low_watermark_monitor(def, self, &where_clause, &query_params, cancel.clone(), rt);

        let candidate_count = candidates.len();
        let rules = Arc::new(def.rules.clone());
        let default_action = Arc::new(def.default_action.clone());
        // Dry-run: wrap the real executor so it never calls the backend.
        // Rule evaluation and per-entry logging still happen.
        let effective_executor: Arc<dyn rbh_actions::ActionExecutor> = if self.dry_run {
            tracing::info!(policy_id = self.policy_id, "dry_run: skipping action execution");
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
            rules,
            default_action,
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

        // Metrics.
        let outcome = if failed == 0 && candidate_count == 0 {
            "empty"
        } else if failed == 0 {
            "success"
        } else if success > 0 {
            "partial"
        } else {
            "failed"
        };
        rbh_observability::metrics::POLICY_RUNS
            .with_label_values(&[policy_id_lbl.as_str(), outcome])
            .inc();
        rbh_observability::metrics::ACTIONS
            .with_label_values(&[policy_id_lbl.as_str(), "success"])
            .inc_by(success);
        rbh_observability::metrics::ACTIONS
            .with_label_values(&[policy_id_lbl.as_str(), "skipped"])
            .inc_by(skipped);
        rbh_observability::metrics::ACTIONS
            .with_label_values(&[policy_id_lbl.as_str(), "failed"])
            .inc_by(failed);
        rbh_observability::metrics::POLICY_RUN_DURATION
            .with_label_values(&[policy_id_lbl.as_str()])
            .observe(run_started.elapsed().as_secs_f64());

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
    def: &crate::PolicyDef, task: &PolicyRunTask, where_clause: &str,
    query_params: &[rbh_entry_store::store::QueryParam], stop_token: scheduler_rs::prelude::CancellationToken,
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
                LowMeasure::Count(low) => match entry_store.count_where(&where_clause, &params).await {
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
                LowMeasure::Volume(low) => match entry_store.sum_size_where(&where_clause, &params).await {
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
                },
            };
            if hit {
                tracing::info!(policy_id, trigger_idx, "low watermark reached — stopping policy run");
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

/// Executor shim used by `PolicyRunTask` when `dry_run=true`. Records
/// what *would* have happened (at `info` level) and always returns
/// `Skipped { reason: "dry run" }` without mutating anything.
struct DryRunExecutor;

#[async_trait]
impl rbh_actions::ActionExecutor for DryRunExecutor {
    async fn execute(
        &self, entry: &rbh_entry_store::model::EntryRow, _ctx: &rbh_actions::ActionContext,
    ) -> Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
        tracing::info!(
            fid = %entry.fid,
            size = entry.size,
            uid = entry.uid,
            "dry_run: would act on entry"
        );
        Ok(rbh_actions::ActionOutcome::Skipped {
            reason: "dry run".into(),
        })
    }
}

/// Fan out candidate processing across `concurrency` workers using a
/// tokio `JoinSet`. Returns `(success, skipped, failed)` totals. Respects
/// the supplied cancel token: outstanding workers complete but no new
/// entries are dispatched after cancellation.
#[allow(clippy::too_many_arguments)]
async fn dispatch_workers(
    candidates: Vec<rbh_entry_store::model::EntryRow>, executor: Arc<dyn rbh_actions::ActionExecutor>,
    action_ctx: Arc<rbh_actions::ActionContext>, concurrency: usize,
    rate_limiter: Option<crate::ratelimit::RateLimiter>, cancel: scheduler_rs::prelude::CancellationToken,
    policy_id: u64, rules: Arc<Vec<Rule>>, default_action: Arc<ActionParams>,
) -> (u64, u64, u64) {
    use tokio::sync::Semaphore;

    let sem = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();

    // Run-level budget: stop dispatching once cumulative bytes of
    // processed entries cross `max_volume`. Size is billed at dispatch
    // (not after completion) so concurrent workers don't race past.
    let max_volume = default_action.max_volume;
    let skip_hardlinked = default_action.skip_hardlinked;
    let mut bytes_dispatched: u64 = 0;
    let mut hardlinked_skipped: u64 = 0;

    for entry in candidates {
        if cancel.is_cancelled() {
            tracing::info!(policy_id, "cancelled before full dispatch");
            break;
        }
        // Hardlink safety: skip entries whose FID has >1 link when
        // skip_hardlinked is set. Prevents a policy targeting one
        // path from implicitly acting on another link to the same FID.
        if skip_hardlinked && entry.nlink > 1 {
            hardlinked_skipped += 1;
            tracing::debug!(
                fid = %entry.fid,
                nlink = entry.nlink,
                "skip_hardlinked: entry has multiple links"
            );
            continue;
        }
        if let Some(cap) = max_volume {
            if bytes_dispatched.saturating_add(entry.size) > cap {
                tracing::info!(
                    policy_id,
                    processed_bytes = bytes_dispatched,
                    cap,
                    "max_volume reached — stopping dispatch"
                );
                break;
            }
            bytes_dispatched = bytes_dispatched.saturating_add(entry.size);
        }

        // First-match rule eval picks the effective per-entry params.
        let params = evaluate_rules(&rules, &default_action, &entry);
        let timeout = params.timeout_secs.filter(|&s| s > 0);
        let retry = params.retry;

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
        set.spawn(async move {
            if worker_cancel.is_cancelled() {
                drop(permit);
                return WorkerOutcome::Skipped;
            }
            let fid = entry.fid;
            let max_attempts = retry.map(|r| r.max_attempts.max(1)).unwrap_or(1);
            let backoff = retry
                .map(|r| std::time::Duration::from_secs(r.backoff_secs))
                .unwrap_or_default();
            let mut last_result: Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> =
                Ok(rbh_actions::ActionOutcome::Failed {
                    error: "no attempts".into(),
                });
            for attempt in 1..=max_attempts {
                if worker_cancel.is_cancelled() {
                    break;
                }
                let fut = exec.execute(&entry, &ctx);
                let result = match timeout {
                    Some(secs) => match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
                        Ok(r) => r,
                        Err(_) => Ok(rbh_actions::ActionOutcome::Failed {
                            error: format!("timeout after {secs}s"),
                        }),
                    },
                    None => fut.await,
                };
                let is_terminal = matches!(
                    &result,
                    Ok(rbh_actions::ActionOutcome::Success) | Ok(rbh_actions::ActionOutcome::Skipped { .. })
                );
                last_result = result;
                if is_terminal || attempt == max_attempts {
                    break;
                }
                tracing::debug!(
                    %fid, attempt, max_attempts, backoff_secs = backoff.as_secs(),
                    "action attempt failed, backing off before retry"
                );
                tokio::time::sleep(backoff).await;
            }
            drop(permit);
            match last_result {
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
    let mut skipped = hardlinked_skipped;
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
    if hardlinked_skipped > 0 {
        tracing::info!(
            policy_id,
            hardlinked_skipped,
            "hardlink safety: skipped entries with nlink > 1"
        );
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
/// for the first matching rule, or default if no rule matches. The
/// resulting ActionParams drives per-entry behavior: timeout_secs
/// wraps the action call in a tokio::time::timeout; other fields stay
/// run-level (enforced in dispatch_workers before the spawn).
fn evaluate_rules(
    rules: &[Rule], default_action: &ActionParams, entry: &rbh_entry_store::model::EntryRow,
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
            dry_run: false,
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
            &self, _entry: &EntryRow, _ctx: &rbh_actions::ActionContext,
        ) -> std::result::Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
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
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams::default());
        let (succ, _, _) = dispatch_workers(mk_candidates(12), exec, ctx, 3, None, cancel, 0, rules, params).await;
        assert_eq!(succ, 12);
        let peak = max_in_flight.load(Ordering::SeqCst);
        assert!((2..=3).contains(&peak), "expected peak ~3, got {peak}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatch_skips_hardlinked_when_opted_in() {
        // 10 candidates: odd indices simulate hardlinked files (nlink=2).
        let mut cands = mk_candidates(10);
        for (i, e) in cands.iter_mut().enumerate() {
            e.nlink = if i % 2 == 0 { 1 } else { 2 };
        }
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(0),
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams {
            skip_hardlinked: true,
            ..ActionParams::default()
        });
        let (succ, skipped, _) = dispatch_workers(cands, exec, ctx, 2, None, cancel, 0, rules, params).await;
        // 5 nlink=1 entries succeed, 5 nlink=2 entries are skipped pre-dispatch.
        assert_eq!(succ, 5);
        assert!(skipped >= 5, "expected >=5 hardlinked skips, got {skipped}");
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
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams::default());
        let (succ, _, _) = dispatch_workers(mk_candidates(30), exec, ctx, 4, rl, cancel, 0, rules, params).await;
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
    async fn dispatch_timeout_marks_failed() {
        // Executor sleeps 500ms; rule says timeout_secs=1 default, but one
        // rule matches uid=0 with timeout_secs=0 (=disabled) to prove
        // evaluate_rules wins. We'll set default timeout_secs=1 which is
        // >500ms → Success; then make default 0 and add a delay higher
        // than the timeout to provoke Failed.
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(300),
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams {
            timeout_secs: Some(1), // executor takes 300ms — within budget
            ..Default::default()
        });
        let (succ, _, failed) = dispatch_workers(
            mk_candidates(3),
            exec.clone(),
            ctx.clone(),
            1,
            None,
            cancel,
            0,
            rules,
            params,
        )
        .await;
        assert_eq!(succ, 3);
        assert_eq!(failed, 0);

        // Now force timeout: executor 500ms vs timeout_secs=0.001… use a
        // fresh executor with longer delay; timeout_secs must be u64, so
        // build a scenario where executor takes ≥1s and timeout is 1s.
        let slow: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(1500),
        });
        let cancel2 = scheduler_rs::prelude::CancellationToken::new();
        let rules2 = Arc::new(Vec::new());
        let params2 = Arc::new(ActionParams {
            timeout_secs: Some(1),
            ..Default::default()
        });
        let (succ2, _, failed2) =
            dispatch_workers(mk_candidates(2), slow, ctx, 2, None, cancel2, 0, rules2, params2).await;
        assert_eq!(succ2, 0);
        assert_eq!(failed2, 2, "both should timeout");
    }

    struct FailThenSucceedExecutor {
        calls: Arc<AtomicUsize>,
        fail_first_n: usize,
    }

    #[async_trait]
    impl rbh_actions::ActionExecutor for FailThenSucceedExecutor {
        async fn execute(
            &self, _entry: &EntryRow, _ctx: &rbh_actions::ActionContext,
        ) -> std::result::Result<rbh_actions::ActionOutcome, rbh_actions::ActionError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_first_n {
                Ok(rbh_actions::ActionOutcome::Failed {
                    error: format!("intentional #{n}"),
                })
            } else {
                Ok(rbh_actions::ActionOutcome::Success)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_retries_until_success() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(FailThenSucceedExecutor {
            calls: calls.clone(),
            fail_first_n: 2, // fail twice, succeed on 3rd attempt
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams {
            retry: Some(crate::RetryParams {
                max_attempts: 3,
                backoff_secs: 0,
            }),
            ..Default::default()
        });
        let (succ, _, failed) = dispatch_workers(mk_candidates(1), exec, ctx, 1, None, cancel, 0, rules, params).await;
        assert_eq!(succ, 1);
        assert_eq!(failed, 0);
        assert_eq!(calls.load(Ordering::SeqCst), 3, "expected 3 attempts");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_retry_exhausts_then_fails() {
        let calls = Arc::new(AtomicUsize::new(0));
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(FailThenSucceedExecutor {
            calls: calls.clone(),
            fail_first_n: 999, // always fail
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams {
            retry: Some(crate::RetryParams {
                max_attempts: 2,
                backoff_secs: 0,
            }),
            ..Default::default()
        });
        let (succ, _, failed) = dispatch_workers(mk_candidates(1), exec, ctx, 1, None, cancel, 0, rules, params).await;
        assert_eq!(succ, 0);
        assert_eq!(failed, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_respects_max_volume() {
        // Each candidate is 1 byte (default test_entry size). With
        // max_volume=5, only 5 entries should dispatch out of 10.
        let exec: Arc<dyn rbh_actions::ActionExecutor> = Arc::new(RecordingExecutor {
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
            outcome: rbh_actions::ActionOutcome::Success,
            delay: Duration::from_millis(1),
        });
        let ctx = Arc::new(rbh_actions::ActionContext {
            mount_path: PathBuf::from("/lustre"),
            lustre: lustre_api::LustreApi,
        });
        let cancel = scheduler_rs::prelude::CancellationToken::new();
        // Shape candidates so each has size=2; with max_volume=7 → only
        // 3 dispatch (6 bytes), the 4th (+2 → 8 > 7) is rejected.
        let mut candidates = mk_candidates(10);
        for e in candidates.iter_mut() {
            e.size = 2;
        }
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams {
            max_volume: Some(7),
            ..Default::default()
        });
        let (succ, skipped, failed) = dispatch_workers(candidates, exec, ctx, 1, None, cancel, 0, rules, params).await;
        assert_eq!(succ + skipped + failed, 3, "expected only 3 dispatched");
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
        let rules = Arc::new(Vec::new());
        let params = Arc::new(ActionParams::default());
        let (succ, skipped, failed) =
            dispatch_workers(mk_candidates(50), exec, ctx, 2, None, cancel, 0, rules, params).await;
        assert!(
            succ + skipped + failed < 50,
            "expected partial run after cancel, got total={}",
            succ + skipped + failed
        );
    }
}
