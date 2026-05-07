//! Reconcile policy triggers with scheduler-rs schedules.
//!
//! Each action policy has a single `trigger` string. On create/update/delete,
//! we remove the existing schedule for that policy and recreate it.

use scheduler_rs::prelude::*;
use scheduler_rs::trigger::*;

use crate::PolicyError;
use crate::model::{TriggerSpec, WindowModeSpec};
use crate::task::PolicyRunTask;
use crate::trigger_parser::parse_trigger;

/// Synchronize a policy's trigger with scheduler-rs.
///
/// 1. Remove any existing schedule for this policy.
/// 2. Parse `trigger_str` and create a new schedule (if time-based).
///    Threshold triggers are driven by the daemon's threshold checker,
///    not by scheduler-rs, so they return `Ok(None)`.
///
/// Returns the new schedule ID, or `None` for threshold triggers.
#[tracing::instrument(skip(scheduler), fields(policy_id))]
pub async fn reconcile_triggers(
    scheduler: &Scheduler, policy_id: u64, trigger_str: &str, enabled: bool,
) -> Result<Option<ScheduleId>, PolicyError> {
    remove_policy_schedule(scheduler, policy_id).await?;

    if !enabled {
        return Ok(None);
    }

    let spec = parse_trigger(trigger_str).map_err(|e| PolicyError::InvalidTrigger(format!("{e}")))?;

    let trigger = match build_trigger(&spec)? {
        Some(t) => t,
        None => return Ok(None), // threshold trigger — daemon-driven
    };

    let task = PolicyRunTask {
        policy_id,
        trigger_idx: 0,
        target: crate::TargetFilter::Fs,
        dry_run: false,
    };
    let task_data = serde_json::to_value(&task).map_err(|e| PolicyError::Scheduler(e.to_string()))?;
    let schedule_name = format!("rbh.policy.{policy_id}");
    let config = ScheduleConfig {
        misfire_policy: MisfirePolicy::Coalesce,
        max_instances: 1,
        ..Default::default()
    };

    let id = scheduler
        .add_raw(
            PolicyRunTask::TYPE_NAME.to_string(),
            task_data,
            trigger,
            config,
            Some(schedule_name),
        )
        .await
        .map_err(|e| PolicyError::Scheduler(e.to_string()))?;

    Ok(Some(id))
}

/// Remove the scheduler-rs schedule for a policy (if any).
#[tracing::instrument(skip(scheduler))]
pub async fn remove_policy_schedule(scheduler: &Scheduler, policy_id: u64) -> Result<(), PolicyError> {
    let prefix = format!("rbh.policy.{policy_id}");
    let schedules = scheduler
        .list_schedules_by_name_prefix(&prefix)
        .await
        .map_err(|e| PolicyError::Scheduler(e.to_string()))?;
    for sched in &schedules {
        scheduler
            .remove(&sched.id)
            .await
            .map_err(|e| PolicyError::Scheduler(e.to_string()))?;
    }
    Ok(())
}

/// Convert a `TriggerSpec` into a boxed scheduler-rs `Trigger`.
/// Returns `Ok(None)` for threshold variants (daemon-driven).
fn build_trigger(spec: &TriggerSpec) -> Result<Option<Box<dyn scheduler_rs::trigger::Trigger>>, PolicyError> {
    match spec {
        TriggerSpec::Interval { secs } => {
            if *secs == 0 {
                return Err(PolicyError::InvalidTrigger("interval must be > 0 seconds".into()));
            }
            Ok(Some(Box::new(IntervalTrigger::every(std::time::Duration::from_secs(
                *secs,
            )))))
        }
        TriggerSpec::Cron { expression } => {
            let trigger = CronTrigger::new(expression).map_err(|e| PolicyError::InvalidTrigger(e.to_string()))?;
            Ok(Some(Box::new(trigger)))
        }
        TriggerSpec::Once { at } => Ok(Some(Box::new(OnceTrigger::at(*at)))),
        TriggerSpec::Immediate => Ok(Some(Box::new(ImmediateTrigger::new()))),
        TriggerSpec::Window { start, end, mode } => {
            let mut w = WindowTrigger::daily().start_at(*start).end_at(*end);
            if let WindowModeSpec::Repeat { interval_secs } = mode {
                if *interval_secs == 0 {
                    return Err(PolicyError::InvalidTrigger(
                        "window repeat interval must be > 0 seconds".into(),
                    ));
                }
                w = w.repeat(std::time::Duration::from_secs(*interval_secs));
            }
            Ok(Some(Box::new(w)))
        }
        TriggerSpec::ThresholdCount { .. }
        | TriggerSpec::ThresholdVolume { .. }
        | TriggerSpec::ThresholdOstPct { .. }
        | TriggerSpec::ThresholdFsPct { .. } => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_triggers_return_none() {
        let specs = vec![
            TriggerSpec::ThresholdCount {
                check_interval_secs: 60,
                high_count: 1000,
                low_count: 0,
                post_trigger_wait_secs: 0,
                target: Default::default(),
            },
            TriggerSpec::ThresholdFsPct {
                check_interval_secs: 300,
                high_pct: 85,
                low_pct: 0,
                post_trigger_wait_secs: 0,
            },
        ];
        for spec in specs {
            assert!(build_trigger(&spec).unwrap().is_none());
        }
    }

    #[test]
    fn interval_trigger_builds() {
        let spec = TriggerSpec::Interval { secs: 300 };
        let t = build_trigger(&spec).unwrap().unwrap();
        assert!(t.description().contains("300"));
    }

    #[test]
    fn zero_interval_rejected() {
        let spec = TriggerSpec::Interval { secs: 0 };
        assert!(build_trigger(&spec).is_err());
    }

    #[test]
    fn cron_trigger_builds() {
        let spec = TriggerSpec::Cron {
            expression: "0 0 2 * * *".into(),
        };
        let t = build_trigger(&spec).unwrap().unwrap();
        assert!(!t.description().is_empty());
    }

    #[test]
    fn invalid_cron_rejected() {
        let spec = TriggerSpec::Cron {
            expression: "not a cron".into(),
        };
        assert!(build_trigger(&spec).is_err());
    }
}
