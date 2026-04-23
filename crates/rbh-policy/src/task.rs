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
        let executor: Box<dyn rbh_actions::ActionExecutor> = match def.kind {
            crate::PolicyKind::Purge => Box::new(rbh_actions::PurgeExecutor),
            crate::PolicyKind::HsmArchive => {
                Box::new(rbh_actions::HsmArchiveExecutor { archive_id: 1 })
            }
            crate::PolicyKind::HsmRelease => Box::new(rbh_actions::HsmReleaseExecutor),
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

        // 4. For each candidate: evaluate rules, dispatch action.
        let action_ctx = rbh_actions::ActionContext {
            mount_path: rt.mount_path.clone(),
            lustre: lustre_api::LustreApi,
        };

        let mut success = 0u64;
        let mut skipped = 0u64;
        let mut failed = 0u64;

        for entry in &candidates {
            if ctx.cancellation_token.is_cancelled() {
                tracing::info!(policy_id = self.policy_id, "cancelled mid-run");
                break;
            }

            // ignore_fileclass is enforced at the SQL layer before this loop
            // (see step 3); rule evaluation here only picks action params.
            let _params = evaluate_rules(&def.rules, &def.default_action, entry);

            match executor.execute(entry, &action_ctx).await {
                Ok(rbh_actions::ActionOutcome::Success) => {
                    success += 1;
                    tracing::debug!(fid = %entry.fid, "action success");
                }
                Ok(rbh_actions::ActionOutcome::Skipped { reason }) => {
                    skipped += 1;
                    tracing::debug!(fid = %entry.fid, reason = %reason, "action skipped");
                }
                Ok(rbh_actions::ActionOutcome::Failed { error }) => {
                    failed += 1;
                    tracing::warn!(fid = %entry.fid, error = %error, "action failed");
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(fid = %entry.fid, error = %e, "action error");
                }
            }
        }

        tracing::info!(
            policy_id = self.policy_id,
            candidates = candidates.len(),
            success,
            skipped,
            failed,
            "policy run completed"
        );

        Ok(())
    }
}

/// Evaluate rules against an entry. Returns effective action params
/// for the first matching rule, or default if no rule matches.
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
}
