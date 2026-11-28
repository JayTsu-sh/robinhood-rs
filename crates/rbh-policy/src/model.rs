//! Classifier schedule syntax preserved for classifier definitions.

#[derive(Debug, Clone, PartialEq)]
pub enum TriggerSpec {
    Interval {
        secs: u64,
    },
    Cron {
        expression: String,
    },
    ThresholdCount {
        check_interval_secs: u64,
        high_count: u64,
        low_count: u64,
        post_trigger_wait_secs: u64,
    },
    ThresholdVolume {
        check_interval_secs: u64,
        high_bytes: u64,
        low_bytes: u64,
        post_trigger_wait_secs: u64,
    },
    ThresholdOstPct {
        check_interval_secs: u64,
        high_pct: u32,
        low_pct: u32,
        post_trigger_wait_secs: u64,
    },
    ThresholdFsPct {
        check_interval_secs: u64,
        high_pct: u32,
        low_pct: u32,
        post_trigger_wait_secs: u64,
    },
}
