use std::collections::{BTreeMap, BTreeSet};

use lustre_api::{
    mdt::SpaceUsage,
    pools::{MountIdentity, OstCapacity, OstTargets},
};
use serde::{Deserialize, Serialize};

use crate::{observation::PoolResource, unix_now};

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CapacityStatus {
    Collected,
    Partial,
    Missing,
    Error,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Capacity {
    status: CapacityStatus,
    attempted_at: Option<u64>,
    sample: Option<CapacitySample>,
    reason: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapacitySample {
    observed_at: u64,
    total_bytes: u64,
    used_bytes: u64,
    available_bytes: u64,
    used_ratio: f64,
}

impl Capacity {
    pub(super) fn pending() -> Self {
        Self {
            status: CapacityStatus::Missing,
            attempted_at: None,
            sample: None,
            reason: None,
        }
    }

    pub(super) fn unavailable(status: CapacityStatus, reason: String) -> Self {
        Self {
            status,
            attempted_at: Some(unix_now()),
            sample: None,
            reason: Some(reason),
        }
    }

    fn measured(usage: SpaceUsage, observed_at: u64) -> Self {
        Self {
            status: CapacityStatus::Collected,
            attempted_at: Some(observed_at),
            sample: Some(CapacitySample {
                observed_at,
                total_bytes: usage.total_bytes,
                used_bytes: usage.used_bytes,
                available_bytes: usage.available_bytes,
                used_ratio: usage.used_ratio(),
            }),
            reason: None,
        }
    }

    pub(super) fn is_collected(&self) -> bool {
        self.status == CapacityStatus::Collected
    }

    fn apply_history(&mut self, previous: &mut Option<CapacitySample>) {
        if let Some(sample) = self.sample {
            *previous = Some(sample);
        } else {
            self.sample = *previous;
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct CapacityHistory {
    mount_id: Option<MountIdentity>,
    filesystem: Option<CapacityHistoryEntry>,
    pools: BTreeMap<String, CapacityHistoryEntry>,
}

#[derive(Serialize, Deserialize)]
struct CapacityHistoryEntry {
    members: BTreeSet<u32>,
    sample: Option<CapacitySample>,
}

impl CapacityHistory {
    pub(super) fn apply(
        &mut self, mount_id: Option<MountIdentity>, targets: Option<BTreeSet<u32>>, capacity: &mut Capacity,
        pools: Option<&mut [PoolResource]>,
    ) {
        let Some(mount_id) = mount_id else { return };
        if self.mount_id.as_ref() != Some(&mount_id) {
            *self = Self {
                mount_id: Some(mount_id),
                ..Self::default()
            };
        }
        if let Some(members) = targets {
            let mut sample = self
                .filesystem
                .take()
                .filter(|prior| prior.members == members)
                .and_then(|prior| prior.sample);
            capacity.apply_history(&mut sample);
            self.filesystem = Some(CapacityHistoryEntry { members, sample });
        }
        let Some(pools) = pools else { return };
        let mut previous = std::mem::take(&mut self.pools);
        for pool in pools {
            let prior = previous.remove(&pool.name);
            let Some(members) = &pool.ost_indices else {
                if let Some(prior) = prior {
                    self.pools.insert(pool.name.clone(), prior);
                }
                continue;
            };
            let mut sample = prior
                .filter(|prior| &prior.members == members)
                .and_then(|prior| prior.sample);
            pool.capacity.apply_history(&mut sample);
            self.pools.insert(
                pool.name.clone(),
                CapacityHistoryEntry {
                    members: members.clone(),
                    sample,
                },
            );
        }
    }
}

pub(super) enum ProbeFailure {
    TimedOut,
    Failed,
    Lustre(String),
}

impl ProbeFailure {
    pub(super) fn code(&self) -> &str {
        match self {
            Self::TimedOut => "probe_timed_out",
            Self::Failed => "probe_failed",
            Self::Lustre(reason) => reason,
        }
    }

    fn description(&self) -> &str {
        match self {
            Self::TimedOut => "容量查询超时",
            Self::Failed => "容量查询失败",
            Self::Lustre(reason) => reason,
        }
    }
}

pub(super) enum CapacityProgress {
    Targets(Result<OstTargets, String>),
    Ost(u32, Result<OstCapacity, String>),
}

pub(super) fn summarize(
    progress: impl IntoIterator<Item = CapacityProgress>, pools: &mut [PoolResource],
    probe_error: Option<&ProbeFailure>, observed_at: u64,
) -> (Capacity, Option<BTreeSet<u32>>) {
    let mut targets = None;
    let mut measurements = BTreeMap::new();
    for event in progress {
        match event {
            CapacityProgress::Targets(indices) => targets = Some(indices),
            CapacityProgress::Ost(index, usage) => {
                measurements.insert(index, usage);
            }
        }
    }
    for pool in pools {
        if let Some(members) = &pool.ost_indices {
            pool.capacity = capacity_for_osts(members, &measurements, probe_error, observed_at);
        }
    }
    match targets {
        Some(Ok(targets)) => {
            let mut members = targets.known;
            let mut inventory_verified = true;
            for index in targets.unverified {
                match measurements.get(&index) {
                    Some(Ok(OstCapacity::Absent)) => continue,
                    Some(Ok(_)) => {}
                    _ => inventory_verified = false,
                }
                members.insert(index);
            }
            let capacity = if members.is_empty() {
                Capacity::unavailable(CapacityStatus::Missing, "no_registered_osts".to_owned())
            } else {
                capacity_for_osts(&members, &measurements, probe_error, observed_at)
            };
            (capacity, inventory_verified.then_some(members))
        }
        Some(Err(error)) => (Capacity::unavailable(CapacityStatus::Error, error), None),
        None => (
            Capacity::unavailable(
                CapacityStatus::Error,
                probe_error.unwrap_or(&ProbeFailure::Failed).code().to_owned(),
            ),
            None,
        ),
    }
}

fn capacity_for_osts(
    members: &BTreeSet<u32>, measurements: &BTreeMap<u32, Result<OstCapacity, String>>,
    probe_error: Option<&ProbeFailure>, observed_at: u64,
) -> Capacity {
    if members.is_empty() {
        return Capacity::measured(SpaceUsage::default(), observed_at);
    }
    let mut total_bytes = 0_u64;
    let mut used_bytes = 0_u64;
    let mut available_bytes = 0_u64;
    let mut measured = 0;
    let mut failure = None;
    for index in members {
        match measurements.get(index) {
            Some(Ok(OstCapacity::Measured(usage))) => {
                let (Some(total), Some(used), Some(available)) = (
                    total_bytes.checked_add(usage.total_bytes),
                    used_bytes.checked_add(usage.used_bytes),
                    available_bytes.checked_add(usage.available_bytes),
                ) else {
                    return Capacity::unavailable(CapacityStatus::Error, "capacity_bytes_overflow".to_owned());
                };
                total_bytes = total;
                used_bytes = used;
                available_bytes = available;
                measured += 1;
            }
            Some(Err(error)) => {
                failure.get_or_insert_with(|| format!("OST{index:04x} · {error}"));
            }
            _ => {}
        }
    }
    if measured == members.len() {
        return Capacity::measured(
            SpaceUsage {
                total_bytes,
                used_bytes,
                available_bytes,
            },
            observed_at,
        );
    }
    let stalled = probe_error.filter(|_| members.iter().any(|index| !measurements.contains_key(index)));
    if measured == 0
        && failure.is_none()
        && let Some(error) = stalled
    {
        return Capacity::unavailable(CapacityStatus::Error, error.code().to_owned());
    }
    let status = if measured > 0 {
        CapacityStatus::Partial
    } else if failure.is_some() {
        CapacityStatus::Error
    } else {
        CapacityStatus::Missing
    };
    let mut reason = format!("已取得 {measured}/{} 个 OST 的容量", members.len());
    let reason_detail = failure.as_deref().or(stalled.map(ProbeFailure::description));
    if let Some(error) = reason_detail {
        reason.push_str(&format!(" · {error}"));
    }
    Capacity::unavailable(status, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_capacity_uses_only_members_and_keeps_the_last_complete_sample() {
        let members = BTreeSet::from([1, 6]);
        let mut measurements = BTreeMap::from([
            (
                1,
                Ok(OstCapacity::Measured(SpaceUsage {
                    total_bytes: 2048,
                    used_bytes: 1024,
                    available_bytes: 512,
                })),
            ),
            (
                6,
                Ok(OstCapacity::Measured(SpaceUsage {
                    total_bytes: 4096,
                    used_bytes: 2048,
                    available_bytes: 1024,
                })),
            ),
            (
                9,
                Ok(OstCapacity::Measured(SpaceUsage {
                    total_bytes: 8192,
                    used_bytes: 512,
                    available_bytes: 2048,
                })),
            ),
        ]);
        let mut previous = None;
        let all_osts = capacity_for_osts(&BTreeSet::from([1, 6, 9]), &measurements, None, 100);
        assert_eq!(all_osts.sample.unwrap().total_bytes, 14336);
        let overlapping_pool = capacity_for_osts(&BTreeSet::from([6]), &measurements, None, 100);
        assert_eq!(overlapping_pool.sample.unwrap().total_bytes, 4096);
        let mut first = capacity_for_osts(&members, &measurements, None, 100);
        first.apply_history(&mut previous);
        let complete = first.sample.unwrap();
        assert_eq!(
            (complete.total_bytes, complete.used_bytes, complete.available_bytes),
            (6144, 3072, 1536)
        );
        assert_eq!(complete.used_ratio, 0.5);

        measurements.insert(6, Ok(OstCapacity::Unavailable));
        let mut partial = capacity_for_osts(&members, &measurements, None, 200);
        assert!(matches!(partial.status, CapacityStatus::Partial));
        assert!(partial.sample.is_none());
        partial.apply_history(&mut previous);
        let historical = partial.sample.unwrap();
        assert_eq!((historical.observed_at, historical.total_bytes), (100, 6144));

        let empty = capacity_for_osts(&BTreeSet::new(), &BTreeMap::new(), Some(&ProbeFailure::TimedOut), 200);
        assert!(matches!(empty.status, CapacityStatus::Collected));
        assert_eq!(empty.sample.unwrap().total_bytes, 0);
    }

    #[test]
    fn an_unknown_mount_or_changed_ost_inventory_does_not_use_an_old_total() {
        let mut history = CapacityHistory::default();
        let initial_targets = BTreeSet::from([1]);
        let mut complete = Capacity::measured(
            SpaceUsage {
                total_bytes: 4096,
                used_bytes: 1024,
                available_bytes: 2048,
            },
            100,
        );
        history.apply(
            Some(MountIdentity::Unique(7)),
            Some(initial_targets.clone()),
            &mut complete,
            None,
        );

        let incomplete = || Capacity::unavailable(CapacityStatus::Partial, "missing OST".to_owned());
        let mut unverified = incomplete();
        history.apply(None, None, &mut unverified, None);
        assert!(unverified.sample.is_none());

        let mut same_targets = incomplete();
        history.apply(
            Some(MountIdentity::Unique(7)),
            Some(initial_targets),
            &mut same_targets,
            None,
        );
        assert_eq!(same_targets.sample.unwrap().total_bytes, 4096);

        let mut new_targets = incomplete();
        history.apply(
            Some(MountIdentity::Unique(7)),
            Some(BTreeSet::from([1, 2])),
            &mut new_targets,
            None,
        );
        assert!(new_targets.sample.is_none());
    }

    #[test]
    fn sparse_slots_are_distinct_from_osts_without_measurements() {
        let targets = || {
            CapacityProgress::Targets(Ok(OstTargets {
                known: BTreeSet::from([0, 2]),
                unverified: BTreeSet::from([1]),
            }))
        };
        let measured = |index| {
            CapacityProgress::Ost(
                index,
                Ok(OstCapacity::Measured(SpaceUsage {
                    total_bytes: 4096,
                    used_bytes: 1024,
                    available_bytes: 2048,
                })),
            )
        };
        let complete = [
            targets(),
            measured(0),
            measured(2),
            CapacityProgress::Ost(1, Ok(OstCapacity::Absent)),
        ];
        let (capacity, members) = summarize(complete, &mut [], None, 100);
        assert_eq!(capacity.sample.unwrap().total_bytes, 8192);
        assert_eq!(members.unwrap(), BTreeSet::from([0, 2]));

        let incomplete = [
            targets(),
            measured(0),
            measured(2),
            CapacityProgress::Ost(1, Ok(OstCapacity::Unavailable)),
        ];
        let (capacity, members) = summarize(incomplete, &mut [], None, 100);
        assert!(matches!(capacity.status, CapacityStatus::Partial));
        assert!(capacity.sample.is_none());
        assert_eq!(members.unwrap(), BTreeSet::from([0, 1, 2]));
    }
}
