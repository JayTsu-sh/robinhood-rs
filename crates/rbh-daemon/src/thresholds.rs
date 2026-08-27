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

use rbh_entry_store::FileSystemId;
use rbh_entry_store::store::{EntryStore, QueryParam};
use rbh_policy::model::ThresholdTarget;
use rbh_policy::{PolicyRow, PolicyRunTask, PolicyStore, TargetFilter, TriggerSpec, parse_trigger};
use rbh_predicate::{Predicate, SqlParam, to_sql};

/// Fire-time bookkeeping: last-fire timestamp per `(policy_id, trigger_idx)`.
type LastFired = Arc<Mutex<HashMap<(u64, u32), u64>>>;

pub struct ThresholdChecker {
    pub policy_store: PolicyStore,
    pub entry_store: EntryStore,
    pub scheduler: Scheduler,
    pub lustre: lustre_api::LustreApi,
    pub filesystem_id: FileSystemId,
    pub backend: rbh_entry_store::BackendKind,
    pub mount_path: std::path::PathBuf,
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

        tracing::info!(filesystem = %self.filesystem_id, tick_secs = self.tick.as_secs(), "threshold checker started");

        loop {
            if self.cancel.is_cancelled() {
                tracing::info!("threshold checker cancelled");
                return;
            }

            if let Err(e) = self.one_cycle(&last_fired, &mut next_check).await {
                tracing::warn!(filesystem = %self.filesystem_id, error = %e, "threshold cycle error");
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
        let Some(config) = self.entry_store.get_filesystem(&self.filesystem_id).await? else {
            anyhow::bail!("unknown filesystem: {}", self.filesystem_id);
        };

        for policy in &policies {
            if !policy.enabled {
                continue;
            }
            if policy.definition.filesystem != self.filesystem_id {
                continue;
            }
            if let Err(error) = rbh_policy::validate_policy_for_filesystem(&policy.definition, &config) {
                tracing::warn!(policy_id = policy.id, filesystem = %self.filesystem_id, %error, "policy capability validation failed before threshold evaluation");
                continue;
            }
            let trigger_spec = match parse_trigger(&policy.definition.trigger) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let idx: u32 = 0;
            {
                let params = match decode_threshold(&trigger_spec) {
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
                if let Err(error) = rbh_policy::validate_target_for_filesystem(&target, &config) {
                    tracing::warn!(policy_id = policy.id, filesystem = %self.filesystem_id, %error, "threshold target is unsupported");
                    continue;
                }
                let tags_pred = Predicate::Tags {
                    match_tags: policy.definition.match_tags.clone(),
                };
                let scope_with_target = match target.to_predicate() {
                    Predicate::True => tags_pred,
                    other => Predicate::And {
                        children: vec![tags_pred, other],
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

                let (fired, fire_target) = match params.measure {
                    Measure::Count { high } => {
                        let c = self
                            .entry_store
                            .count_scoped_where(&self.filesystem_id, &where_clause, &store_params)
                            .await?;
                        tracing::debug!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            count = c,
                            high,
                            "threshold count evaluated"
                        );
                        (c >= high, target.clone())
                    }
                    Measure::Volume { high } => {
                        // SUM(size) WHERE scope — use a one-shot raw query.
                        let v = self
                            .entry_store
                            .sum_scoped_size_where(&self.filesystem_id, &where_clause, &store_params)
                            .await?;
                        tracing::debug!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            total_bytes = v,
                            high,
                            "threshold volume evaluated"
                        );
                        (v >= high, target.clone())
                    }
                    Measure::OstPct { high_pct } => {
                        let usages = match fetch_ost_usage(self, policy.id, idx).await {
                            Some(v) => v,
                            None => continue,
                        };
                        match first_ost_over_pct(&usages, params.target, high_pct) {
                            Some(hot) => {
                                tracing::debug!(
                                    policy_id = policy.id,
                                    trigger_idx = idx,
                                    hot_ost = hot,
                                    high_pct,
                                    "ost threshold fired — narrowing run to hot OST"
                                );
                                (true, TargetFilter::Ost { osts: vec![hot] })
                            }
                            None => {
                                tracing::debug!(
                                    policy_id = policy.id,
                                    trigger_idx = idx,
                                    high_pct,
                                    "no OST exceeds threshold"
                                );
                                (false, target.clone())
                            }
                        }
                    }
                    Measure::FsPct { high_pct } => {
                        // Aggregate all OSTs: sum(used) / sum(total) × 100.
                        let usages = match fetch_ost_usage(self, policy.id, idx).await {
                            Some(v) => v,
                            None => continue,
                        };
                        let total: u64 = usages.iter().map(|u| u.total_bytes).sum();
                        let used: u64 = usages.iter().map(|u| u.used_bytes).sum();
                        let agg_pct = if total == 0 {
                            0.0
                        } else {
                            used as f64 / total as f64 * 100.0
                        };
                        tracing::debug!(
                            policy_id = policy.id,
                            trigger_idx = idx,
                            agg_pct,
                            high_pct,
                            "fs aggregate usage evaluated"
                        );
                        (agg_pct >= high_pct as f64, target.clone())
                    }
                };

                if fired {
                    if let Err(e) = self.fire(policy, idx, &fire_target).await {
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
                            .with_label_values(&[self.filesystem_id.as_str(), self.backend.as_str(), pid_str.as_str()])
                            .inc();
                        tracing::info!(
                            policy_id = policy.id,
                            filesystem = %self.filesystem_id,
                            backend = ?self.backend,
                            trigger_idx = idx,
                            target = ?fire_target,
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
            dry_run: false,
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

#[derive(Debug, PartialEq, Eq)]
enum Measure {
    Count {
        high: u64,
    },
    Volume {
        high: u64,
    },
    /// Live OST usage percentage via `llapi_obd_statfs`. `high_pct` is
    /// 0..=100; the checker fires when any targeted OST exceeds this.
    OstPct {
        high_pct: u32,
    },
    /// Aggregate filesystem usage (sum-used / sum-total across all OSTs).
    /// Equivalent to robinhood-C `trigger_on = global_usage`.
    FsPct {
        high_pct: u32,
    },
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
        TriggerSpec::ThresholdOstPct {
            check_interval_secs,
            high_pct,
            post_trigger_wait_secs,
            target,
            ..
        } => Some(ThresholdParams {
            check_interval_secs: *check_interval_secs,
            post_trigger_wait_secs: *post_trigger_wait_secs,
            target,
            measure: Measure::OstPct { high_pct: *high_pct },
        }),
        TriggerSpec::ThresholdFsPct {
            check_interval_secs,
            high_pct,
            post_trigger_wait_secs,
            ..
        } => Some(ThresholdParams {
            check_interval_secs: *check_interval_secs,
            post_trigger_wait_secs: *post_trigger_wait_secs,
            target: &ThresholdTarget::Fs,
            measure: Measure::FsPct { high_pct: *high_pct },
        }),
        _ => None,
    }
}

/// Fetch OST statfs data on a blocking thread; returns `None` and logs a
/// warning on error so callers can `continue` the polling loop cleanly.
async fn fetch_ost_usage(
    checker: &ThresholdChecker, policy_id: u64, trigger_idx: u32,
) -> Option<Vec<lustre_api::OstUsage>> {
    let lustre = checker.lustre;
    let mount = checker.mount_path.clone();
    match tokio::task::spawn_blocking(move || lustre.ost_usage(&mount)).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            tracing::warn!(policy_id, trigger_idx, error = %e, "ost statfs failed — skipping cycle");
            None
        }
        Err(e) => {
            tracing::warn!(policy_id, trigger_idx, error = %e, "ost_usage join error");
            None
        }
    }
}

/// Return `Some(index)` for the first OST whose `used_ratio * 100 >=
/// high_pct`, restricted to `target`. `Fs` considers every OST; `Ost`
/// restricts to the listed indices; `Pool { name }` does a naive name
/// prefix match (stripped of the `-OSTxxxx` suffix).
fn first_ost_over_pct(
    usages: &[lustre_api::OstUsage], target: &rbh_policy::model::ThresholdTarget, high_pct: u32,
) -> Option<u32> {
    use rbh_policy::model::ThresholdTarget as T;
    let hot = high_pct as f64 / 100.0;
    for u in usages {
        let belongs = match target {
            T::Fs => true,
            T::Ost { osts } => osts.contains(&u.index),
            T::Pool { name } => {
                // Lustre pool membership is recorded out-of-band; we
                // approximate by matching the pool name against the
                // filesystem prefix of the OST name (e.g. "testfs").
                // For a true pool check callers should use Ost{...}.
                u.name.starts_with(name.as_str())
            }
            T::User { .. } | T::Group { .. } => return None,
        };
        if belongs && u.used_ratio() >= hot {
            return Some(u.index);
        }
    }
    None
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

        let o = TriggerSpec::ThresholdOstPct {
            check_interval_secs: 30,
            high_pct: 85,
            low_pct: 60,
            post_trigger_wait_secs: 600,
            target: ThresholdTarget::Fs,
        };
        assert!(matches!(
            decode_threshold(&o).unwrap().measure,
            Measure::OstPct { high_pct: 85 }
        ));
    }

    fn mk_usage(index: u32, name: &str, total: u64, used: u64) -> lustre_api::OstUsage {
        lustre_api::OstUsage {
            index,
            name: name.into(),
            total_bytes: total,
            used_bytes: used,
            free_bytes: total - used,
            avail_bytes: total - used,
        }
    }

    #[test]
    fn ost_pct_fs_target_flags_any_over_threshold() {
        let usages = vec![
            mk_usage(0, "testfs-OST0000", 1_000, 500), // 50%
            mk_usage(1, "testfs-OST0001", 1_000, 900), // 90%
        ];
        assert_eq!(first_ost_over_pct(&usages, &ThresholdTarget::Fs, 85), Some(1));
        assert_eq!(first_ost_over_pct(&usages, &ThresholdTarget::Fs, 95), None);
    }

    #[test]
    fn ost_pct_ost_target_restricts_indices() {
        let usages = vec![
            mk_usage(0, "fs-OST0000", 1_000, 950), // 95%
            mk_usage(1, "fs-OST0001", 1_000, 300),
        ];
        // Even though OST0 is 95%, target says only index 1 counts.
        assert_eq!(
            first_ost_over_pct(&usages, &ThresholdTarget::Ost { osts: vec![1] }, 85),
            None
        );
        assert_eq!(
            first_ost_over_pct(&usages, &ThresholdTarget::Ost { osts: vec![0, 1] }, 85),
            Some(0)
        );
    }

    #[test]
    fn ost_pct_pool_target_prefix_matches_fs_name() {
        let usages = vec![mk_usage(0, "flash-OST0000", 1_000, 900)];
        assert_eq!(
            first_ost_over_pct(&usages, &ThresholdTarget::Pool { name: "flash".into() }, 85),
            Some(0)
        );
        assert_eq!(
            first_ost_over_pct(&usages, &ThresholdTarget::Pool { name: "bulk".into() }, 85),
            None
        );
    }

    #[test]
    fn ost_pct_user_target_rejects() {
        let usages = vec![mk_usage(0, "fs-OST0000", 1_000, 999)];
        // User target isn't meaningful for OST-level thresholds.
        assert_eq!(
            first_ost_over_pct(&usages, &ThresholdTarget::User { uid: 1000 }, 10),
            None
        );
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

#[test]
fn fs_pct_decodes_correctly() {
    let spec = TriggerSpec::ThresholdFsPct {
        check_interval_secs: 120,
        high_pct: 90,
        low_pct: 70,
        post_trigger_wait_secs: 300,
    };
    match decode_threshold(&spec) {
        Some(p) => {
            assert_eq!(p.check_interval_secs, 120);
            assert!(matches!(p.measure, Measure::FsPct { high_pct: 90 }));
        }
        None => panic!("ThresholdFsPct should decode to Some"),
    }
}

#[test]
fn aggregate_pct_computation() {
    use lustre_api::OstUsage;
    // Two OSTs: 50GiB total, 10GiB used → 20%
    let usages: Vec<OstUsage> = vec![
        OstUsage {
            index: 0,
            name: "fs-OST0000".into(),
            total_bytes: 25 << 30,
            used_bytes: 5 << 30,
            free_bytes: 20 << 30,
            avail_bytes: 20 << 30,
        },
        OstUsage {
            index: 1,
            name: "fs-OST0001".into(),
            total_bytes: 25 << 30,
            used_bytes: 5 << 30,
            free_bytes: 20 << 30,
            avail_bytes: 20 << 30,
        },
    ];
    let total: u64 = usages.iter().map(|u| u.total_bytes).sum();
    let used: u64 = usages.iter().map(|u| u.used_bytes).sum();
    let pct = used as f64 / total as f64 * 100.0;
    assert!((pct - 20.0).abs() < 0.01, "expected ~20%, got {pct}");
    // Should NOT fire at high_pct=85
    assert!(pct < 85.0);
}
