//! Prometheus metrics. A single process-wide [`Registry`] hosts every
//! counter / gauge / histogram; crates mutate them through the exported
//! handles without importing `prometheus` directly.
//!
//! Use [`render`] to produce the Prometheus text exposition format for
//! the `/metrics` endpoint.

use once_cell::sync::Lazy;
use prometheus::{IntCounterVec, IntGaugeVec, Opts, Registry};

pub static REGISTRY: Lazy<Registry> = Lazy::new(Registry::new);

/// Current catalog entry count. Refreshed by the `/metrics` handler.
pub static CATALOG_ENTRIES: Lazy<IntGaugeVec> = Lazy::new(|| {
    let g = IntGaugeVec::new(
        Opts::new("rbh_catalog_entries", "Entries currently in the catalog"),
        &["filesystem", "backend"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(g.clone())).ok();
    g
});

/// Changelog events ingested, labeled `mdt` + `event_type`.
pub static CHANGELOG_EVENTS: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "rbh_changelog_events_total",
            "Changelog events ingested per MDT per type",
        ),
        &["filesystem", "backend", "source", "event_type"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

/// Entries scanned by the active-HSM-state poller.
pub static HSM_POLL_SCANNED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "rbh_hsm_poll_scanned_total",
            "Entries inspected by the HSM state poller",
        ),
        &["filesystem", "backend"],
    )
    .expect("metric registration");
    REGISTRY.register(Box::new(c.clone())).ok();
    c
});

/// Entries whose catalog HSM state was patched to match the live MDS
/// state during a poller cycle.
pub static HSM_POLL_RECONCILED: Lazy<IntCounterVec> = Lazy::new(|| {
    let c = IntCounterVec::new(
        Opts::new(
            "rbh_hsm_poll_reconciled_total",
            "Entries whose HSM state was corrected by the poller",
        ),
        &["filesystem", "backend"],
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
        CATALOG_ENTRIES.with_label_values(&["lustre-a", "lustre"]).set(42);
        let out = render().unwrap();
        assert!(out.contains("rbh_catalog_entries"));
        assert!(out.contains("42"));
        assert!(out.contains("filesystem=\"lustre-a\""));
        assert!(out.contains("backend=\"lustre\""));
    }
}
