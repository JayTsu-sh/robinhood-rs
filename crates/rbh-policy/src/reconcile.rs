//! Reconcile policy triggers with scheduler-rs schedules.
//!
//! Each policy trigger maps 1:1 to a scheduler-rs schedule. On policy
//! create/update/delete, we delete all existing schedules for that
//! policy and recreate them from the current trigger list.

use scheduler_rs::prelude::*;

use crate::PolicyError;
use crate::model::{PolicyDef, TriggerSpec, WindowModeSpec};
use crate::task::PolicyRunTask;

/// Synchronize a policy's triggers with scheduler-rs.
///
/// 1. Remove all existing schedules for this policy.
/// 2. For each trigger in the definition, create a new schedule.
///
/// Returns the list of new schedule IDs.
#[tracing::instrument(skip(scheduler, def), fields(policy_name = %def.name))]
pub async fn reconcile_triggers(
    scheduler: &Scheduler, policy_id: u64, def: &PolicyDef,
) -> Result<Vec<ScheduleId>, PolicyError> {
    if !def.enabled {
        remove_policy_schedules(scheduler, policy_id).await?;
        return Ok(vec![]);
    }

    // Step 1: build ALL triggers eagerly before touching the scheduler.
    // This ensures validation failures (bad cron, zero interval) abort
    // before any existing schedules are deleted.
    let mut built_triggers: Vec<(u32, Box<dyn scheduler_rs::trigger::Trigger>)> =
        Vec::with_capacity(def.triggers.len());
    for (idx, trigger_spec) in def.triggers.iter().enumerate() {
        if let Some(trigger) = build_trigger(trigger_spec)? {
            built_triggers.push((idx as u32, trigger));
        }
    }

    // Step 2: remove old schedules (only after validation passes)
    remove_policy_schedules(scheduler, policy_id).await?;

    // Step 3: create new schedules. On partial failure, clean up what was created.
    let mut ids = Vec::with_capacity(built_triggers.len());
    for (idx, trigger) in built_triggers {
        let task = PolicyRunTask {
            policy_id,
            trigger_idx: idx,
            target: crate::TargetFilter::Fs,
        };
        let task_data = serde_json::to_value(&task).map_err(|e| PolicyError::Scheduler(e.to_string()))?;
        let schedule_name = format!("rbh.policy.{}.trigger.{}", policy_id, idx);

        let config = ScheduleConfig {
            misfire_policy: MisfirePolicy::Coalesce,
            max_instances: 1,
            ..Default::default()
        };

        match scheduler
            .add_raw(
                PolicyRunTask::TYPE_NAME.to_string(),
                task_data,
                trigger,
                config,
                Some(schedule_name),
            )
            .await
        {
            Ok(id) => ids.push(id),
            Err(e) => {
                // Rollback: remove any schedules created so far in this batch.
                for created_id in &ids {
                    let _ = scheduler.remove(created_id).await;
                }
                return Err(PolicyError::Scheduler(e.to_string()));
            }
        }
    }

    Ok(ids)
}

/// Remove all scheduler-rs schedules belonging to a policy.
/// Uses server-side name prefix filtering via `list_schedules_by_name_prefix`.
#[tracing::instrument(skip(scheduler))]
pub async fn remove_policy_schedules(scheduler: &Scheduler, policy_id: u64) -> Result<(), PolicyError> {
    let prefix = format!("rbh.policy.{}.trigger.", policy_id);
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
///
/// Returns `Ok(None)` for threshold variants — those are driven by the
/// daemon-level threshold checker, not by scheduler-rs.
fn build_trigger(
    spec: &TriggerSpec,
) -> Result<Option<Box<dyn scheduler_rs::trigger::Trigger>>, PolicyError> {
    use scheduler_rs::trigger::*;

    match spec {
        TriggerSpec::Interval { secs } => {
            if *secs == 0 {
                return Err(PolicyError::InvalidTrigger(
                    "interval must be > 0 seconds".to_string(),
                ));
            }
            Ok(Some(Box::new(IntervalTrigger::every(
                std::time::Duration::from_secs(*secs),
            ))))
        }
        TriggerSpec::Cron { expression } => {
            let trigger = CronTrigger::new(expression)
                .map_err(|e| PolicyError::InvalidTrigger(e.to_string()))?;
            Ok(Some(Box::new(trigger)))
        }
        TriggerSpec::Once { at } => Ok(Some(Box::new(OnceTrigger::at(*at)))),
        TriggerSpec::Immediate => Ok(Some(Box::new(ImmediateTrigger::new()))),
        TriggerSpec::Window { start, end, mode } => {
            let mut w = WindowTrigger::daily().start_at(*start).end_at(*end);
            if let WindowModeSpec::Repeat { interval_secs } = mode {
                if *interval_secs == 0 {
                    return Err(PolicyError::InvalidTrigger(
                        "window repeat interval must be > 0 seconds".to_string(),
                    ));
                }
                w = w.repeat(std::time::Duration::from_secs(*interval_secs));
            }
            Ok(Some(Box::new(w)))
        }
        // Threshold triggers are driven by the daemon's threshold checker,
        // not by scheduler-rs.
        TriggerSpec::ThresholdCount { .. } | TriggerSpec::ThresholdVolume { .. } => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    #[test]
    fn build_interval_trigger() {
        let spec = TriggerSpec::Interval { secs: 300 };
        let trigger = build_trigger(&spec).unwrap().unwrap();
        assert!(trigger.description().contains("300"));
    }

    #[test]
    fn build_cron_trigger() {
        let spec = TriggerSpec::Cron {
            expression: "0 0 2 * * *".to_string(),
        };
        let trigger = build_trigger(&spec).unwrap().unwrap();
        assert!(!trigger.description().is_empty());
    }

    #[test]
    fn build_immediate_trigger() {
        let spec = TriggerSpec::Immediate;
        let trigger = build_trigger(&spec).unwrap().unwrap();
        // Immediate should fire once
        let next = trigger.next_fire_time(&Utc::now());
        assert!(next.is_some());
    }

    #[test]
    fn build_window_trigger_once() {
        let spec = TriggerSpec::Window {
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            mode: WindowModeSpec::Once,
        };
        let trigger = build_trigger(&spec).unwrap().unwrap();
        assert!(!trigger.description().is_empty());
    }

    #[test]
    fn build_window_trigger_repeat() {
        let spec = TriggerSpec::Window {
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            mode: WindowModeSpec::Repeat { interval_secs: 600 },
        };
        let trigger = build_trigger(&spec).unwrap().unwrap();
        assert!(!trigger.description().is_empty());
    }

    #[test]
    fn invalid_cron_rejected() {
        let spec = TriggerSpec::Cron {
            expression: "not a cron".to_string(),
        };
        assert!(build_trigger(&spec).is_err());
    }

    #[test]
    fn zero_interval_rejected() {
        let spec = TriggerSpec::Interval { secs: 0 };
        assert!(build_trigger(&spec).is_err());
    }

    #[test]
    fn zero_window_repeat_rejected() {
        let spec = TriggerSpec::Window {
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            mode: WindowModeSpec::Repeat { interval_secs: 0 },
        };
        assert!(build_trigger(&spec).is_err());
    }

    #[test]
    fn threshold_count_is_skipped_for_scheduler() {
        let spec = TriggerSpec::ThresholdCount {
            check_interval_secs: 60,
            high_count: 1_000,
            low_count: 500,
            post_trigger_wait_secs: 0,
            target: crate::model::ThresholdTarget::Fs,
        };
        assert!(
            matches!(build_trigger(&spec), Ok(None)),
            "threshold triggers must not produce scheduler-rs triggers"
        );
    }
}
