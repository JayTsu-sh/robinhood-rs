//! Runtime evaluation of threshold triggers.
//!
//! scheduler-rs handles time-based triggers (interval/cron/window). Threshold
//! triggers — "when OST pool X exceeds 85%, run purge" — need to re-check a
//! condition periodically. This module runs one background task that:
//!
//! 1. Every `tick` seconds, lists enabled policies.
//! 2. For each `TriggerSpec::ThresholdCount / ThresholdVolume` in the policy,
//!    composes `scope AND <target>` and counts/sums matching entries.
//! 3. If the value ≥ high_watermark and the policy is not in cooldown,
//!    injects an Immediate policy run via `scheduler-rs::add_raw`.
//! 4. Records the fire time to enforce `post_trigger_wait_secs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scheduler_rs::prelude::{MisfirePolicy, ScheduleConfig, Scheduler, Task};
use scheduler_rs::trigger::ImmediateTrigger;
use tokio::sync::Mutex;
use tokio::time;
use tokio_util::sync::CancellationToken;

use rbh_entry_store::store::{EntryStore, QueryParam};
use rbh_policy::model::ThresholdTarget;
use rbh_policy::{PolicyRow, PolicyRunTask, PolicyStore, TargetFilter, TriggerSpec, compose_scope_with_ignores};
use rbh_predicate::{Predicate, SqlParam, to_sql};

/// Fire-time bookkeeping: last-fire timestamp per `(policy_id, trigger_idx)`.
type LastFired = Arc<Mutex<HashMap<(u64, u32), u64>>>;

pub struct ThresholdChecker {
    pub policy_store: PolicyStore,
    pub entry_store: EntryStore,
    pub scheduler: Scheduler,
    /// Minimum sleep between cycles. Each trigger has its own
    /// `check_interval_secs`; the loop wakes at the shortest.
    pub tick: Duration,
    pub cancel: CancellationToken,
}

impl ThresholdChecker {
    /// Run forever until `cancel` fires.
    pub async fn run(self) {
        let last_fired: LastFired = Arc::new(Mutex::new(HashMap::new()));
        // next_check_at[(policy_id, idx)] = epoch second at which this
        // trigger should next be re-evaluated.
        let mut next_check: HashMap<(u64, u32), u64> = HashMap::new();

        tracing::info!(tick_secs = self.tick.as_secs(), "threshold checker started");

        loop {
            if self.cancel.is_cancelled() {
                tracing::info!("threshold checker cancelled");
                return;
            }

            if let Err(e) = self.one_cycle(&last_fired, &mut next_check).await {
                tracing::warn!(error = %e, "threshold cycle error");
            }

            tokio::select! {
                _ = time::sleep(self.tick) => {}
                _ = self.cancel.cancelled() => {
                    tracing::info!("threshold checker cancelled mid-sleep");
                    return;
                }
            }
        }
    }

    async fn one_cycle(&self, last_fired: &LastFired, next_check: &mut HashMap<(u64, u32), u64>) -> anyhow::Result<()> {
        let policies = self.policy_store.list().await?;
        let now = now_secs();

        for policy in &policies {
            if !policy.enabled {
                continue;
            }
            for (idx, trigger) in policy.definition.triggers.iter().enumerate() {
                let idx = idx as u32;
                let params = match decode_threshold(trigger) {
                    Some(p) => p,
                    None => continue, // not a threshold trigger
                };

                let key = (policy.id, idx);
                if *next_check.get(&key).unwrap_or(&0) > now {
                    continue;
                }
                next_check.insert(key, now + params.check_interval_secs);

                let in_cooldown = {
                    let map = last_fired.lock().await;
                    map.get(&key)
                        .map(|last| now < *last + params.post_trigger_wait_secs)
                        .unwrap_or(false)
                };
                if in_cooldown {
                    continue;
                }

                let target = resolve_target(params.target);
                let scope_with_ignore =
                    compose_scope_with_ignores(&policy.definition.scope, &policy.definition.ignore_fileclass);
                let scope_with_target = match target.to_predicate() {
                    Predicate::True => scope_with_ignore,
                    other => Predicate::And {
                        children: vec![scope_with_ignore, other],
                    },
                };
                let (where_clause, sql_params): (String, Vec<SqlParam>) = to_sql(&scope_with_target);
                let store_params: Vec<QueryParam> = sql_params
                    .into_iter()
                    .map(|p| match p {
                        SqlParam::Num(n) => QueryParam::Int(n),
                        SqlParam::Str(s) => QueryParam::Str(s),
                    })
                    .collect();

                let fired = match params.measure {
                    Measure::Count { high } => {
                        let c = self.entry_store.count_where(&where_clause, &store_params).await?;
                        tracing::debug!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            count = c,
                            high,
                            "threshold count evaluated"
                        );
                        c >= high
                    }
                    Measure::Volume { high } => {
                        // SUM(size) WHERE scope — use a one-shot raw query.
                        let v = sum_size_where(&self.entry_store, &where_clause, &store_params).await?;
                        tracing::debug!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            total_bytes = v,
                            high,
                            "threshold volume evaluated"
                        );
                        v >= high
                    }
                };

                if fired {
                    if let Err(e) = self.fire(policy, idx, &target).await {
                        tracing::warn!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            error = %e,
                            "threshold fire failed"
                        );
                    } else {
                        last_fired.lock().await.insert(key, now);
                        let pid_str = policy.id.to_string();
                        rbh_observability::metrics::THRESHOLD_FIRES
                            .with_label_values(&[pid_str.as_str()])
                            .inc();
                        tracing::info!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            target = ?target,
                            "threshold triggered — policy run scheduled"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    async fn fire(&self, policy: &PolicyRow, trigger_idx: u32, target: &TargetFilter) -> anyhow::Result<()> {
        let task = PolicyRunTask {
            policy_id: policy.id,
            trigger_idx,
            target: target.clone(),
        };
        let task_data = serde_json::to_value(&task)?;
        let config = ScheduleConfig {
            misfire_policy: MisfirePolicy::Coalesce,
            max_instances: 1,
            ..Default::default()
        };
        let schedule_name = format!("rbh.policy.{}.threshold.{}.{}", policy.id, trigger_idx, now_secs());
        self.scheduler
            .add_raw(
                PolicyRunTask::TYPE_NAME.to_string(),
                task_data,
                Box::new(ImmediateTrigger::new()),
                config,
                Some(schedule_name),
            )
            .await
            .map_err(|e| anyhow::anyhow!("scheduler add_raw: {e}"))?;
        Ok(())
    }
}

// ───────────────────────────── helpers ────────────────────────────────

struct ThresholdParams<'a> {
    check_interval_secs: u64,
    post_trigger_wait_secs: u64,
    target: &'a ThresholdTarget,
    measure: Measure,
}

enum Measure {
    Count { high: u64 },
    Volume { high: u64 },
}

fn decode_threshold(spec: &TriggerSpec) -> Option<ThresholdParams<'_>> {
    match spec {
        TriggerSpec::ThresholdCount {
            check_interval_secs,
            high_count,
            post_trigger_wait_secs,
            target,
            ..
        } => Some(ThresholdParams {
            check_interval_secs: *check_interval_secs,
            post_trigger_wait_secs: *post_trigger_wait_secs,
            target,
            measure: Measure::Count { high: *high_count },
        }),
        TriggerSpec::ThresholdVolume {
            check_interval_secs,
            high_bytes,
            post_trigger_wait_secs,
            target,
            ..
        } => Some(ThresholdParams {
            check_interval_secs: *check_interval_secs,
            post_trigger_wait_secs: *post_trigger_wait_secs,
            target,
            measure: Measure::Volume { high: *high_bytes },
        }),
        _ => None,
    }
}

fn resolve_target(t: &ThresholdTarget) -> TargetFilter {
    match t {
        ThresholdTarget::Fs => TargetFilter::Fs,
        ThresholdTarget::Ost { osts } => TargetFilter::Ost { osts: osts.clone() },
        ThresholdTarget::Pool { name } => TargetFilter::Pool { name: name.clone() },
        ThresholdTarget::User { uid } => TargetFilter::User { uid: *uid },
        ThresholdTarget::Group { gid } => TargetFilter::Group { gid: *gid },
    }
}

/// Total size across all matching entries. Separated from EntryStore to
/// avoid bloating its public API with a one-off helper.
async fn sum_size_where(store: &EntryStore, where_clause: &str, params: &[QueryParam]) -> anyhow::Result<u64> {
    use sqlx::Row;
    let sql = format!(
        "SELECT CAST(COALESCE(SUM(size), 0) AS UNSIGNED) AS total \
         FROM entries WHERE {where_clause}"
    );
    let mut q = sqlx::query(&sql);
    for p in params {
        q = match p {
            QueryParam::Int(n) => q.bind(*n),
            QueryParam::Str(s) => q.bind(s.as_str()),
        };
    }
    let row = q.fetch_one(store.pool()).await?;
    Ok(row.try_get::<u64, _>("total").unwrap_or(0))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_distinguishes_threshold_variants() {
        let c = TriggerSpec::ThresholdCount {
            check_interval_secs: 60,
            high_count: 1000,
            low_count: 0,
            post_trigger_wait_secs: 0,
            target: ThresholdTarget::Fs,
        };
        let v = TriggerSpec::ThresholdVolume {
            check_interval_secs: 60,
            high_bytes: 1 << 30,
            low_bytes: 0,
            post_trigger_wait_secs: 0,
            target: ThresholdTarget::Fs,
        };
        let interval = TriggerSpec::Interval { secs: 60 };
        assert!(matches!(
            decode_threshold(&c).unwrap().measure,
            Measure::Count { high: 1000 }
        ));
        assert!(matches!(
            decode_threshold(&v).unwrap().measure,
            Measure::Volume { high: n } if n == 1 << 30
        ));
        assert!(decode_threshold(&interval).is_none());
    }

    #[test]
    fn resolve_target_maps_pool_and_ost() {
        assert_eq!(resolve_target(&ThresholdTarget::Fs), TargetFilter::Fs);
        assert_eq!(
            resolve_target(&ThresholdTarget::Pool { name: "flash".into() }),
            TargetFilter::Pool { name: "flash".into() }
        );
        assert_eq!(
            resolve_target(&ThresholdTarget::Ost { osts: vec![1, 2] }),
            TargetFilter::Ost { osts: vec![1, 2] }
        );
        assert_eq!(
            resolve_target(&ThresholdTarget::User { uid: 1000 }),
            TargetFilter::User { uid: 1000 }
        );
    }
}
