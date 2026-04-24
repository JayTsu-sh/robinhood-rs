//! Prometheus metrics. A single process-wide [`Registry`] hosts every
//! counter / gauge / histogram; crates mutate them through the exported
//! handles without importing `prometheus` directly.
//!
//! Use [`render`] to produce the Prometheus text exposition format for
//! the `/metrics` endpoint.

use once_cell::sync::Lazy;
use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry};

pub static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

/// Current catalog entry count. Refreshed by the `/metrics` handler.
pub static CATALOG_ENTRIES: Lazy<IntGauge> = Lazy::new(|| {
    let g = IntGauge::new("rbh_catalog_entries", "Entries currently in the catalog").expect("metric registration");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

/// Policy runs completed, labeled `policy_id` + `outcome`
/// (`success`/`partial`/`failed`/`error`).
pub static POLICY_RUNS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("rbh_policy_runs_total", "Policy runs completed"),
        &["policy_id", "outcome"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

/// Per-run wall-clock duration, labeled by `policy_id`.
pub static POLICY_RUN_DURATION: Lazy<HistogramVec> = Lazy::new(|| {
    let h = HistogramVec::new(
        HistogramOpts::new("rbh_policy_run_duration_seconds", "Policy run wall-clock duration")
            .buckets(vec![0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 3600.0]),
        &["policy_id"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(h.clone())).ok();
    h
});

/// Threshold trigger fire events, labeled `policy_id`.
pub static THRESHOLD_FIRES: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("rbh_threshold_fires_total", "Threshold trigger fires"),
        &["policy_id"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

/// Changelog events ingested, labeled `mdt` + `event_type`.
pub static CHANGELOG_EVENTS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "rbh_changelog_events_total",
            "Changelog events ingested per MDT per type",
        ),
        &["mdt", "event_type"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

/// Individual action outcomes (dispatched under a policy run).
pub static ACTIONS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new("rbh_actions_total", "Per-entry action outcomes"),
        &["policy_id", "outcome"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

/// Render the current metrics snapshot as Prometheus text exposition.
pub fn render() -> Result<String, prometheus::Error> {
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    encoder.encode(&REGISTRY.gather(), &mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_registered_families() {
        CATALOG_ENTRIES.set(42);
        POLICY_RUNS.with_label_values(&["7", "success"]).inc();
        THRESHOLD_FIRES.with_label_values(&["7"]).inc_by(3);
        let out = render().unwrap();
        assert!(out.contains("rbh_catalog_entries"));
        assert!(out.contains("rbh_policy_runs_total"));
        assert!(out.contains("rbh_threshold_fires_total"));
        assert!(out.contains("42"));
    }
}
