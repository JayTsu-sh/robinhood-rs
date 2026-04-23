# Grafana dashboard

`robinhood-rs.json` is an importable dashboard for the metrics exposed
at `/api/metrics` by the rbh-daemon.

## Prerequisites

1. **Prometheus** scraping the daemon:

   ```yaml
   # /etc/prometheus/prometheus.yml
   scrape_configs:
     - job_name: robinhood-rs
       metrics_path: /api/metrics
       static_configs:
         - targets: ['rbh-daemon-host:8080']
   ```

2. **Grafana** with a Prometheus datasource.

## Install

Grafana UI → Dashboards → *Import* → upload `robinhood-rs.json` →
select the Prometheus datasource → *Import*.

## Panels

| Panel | PromQL |
|-------|--------|
| Catalog entries | `rbh_catalog_entries` |
| Threshold fires (5m) | `sum(increase(rbh_threshold_fires_total[5m]))` |
| Policy runs (1h) | `sum(increase(rbh_policy_runs_total[1h]))` |
| Failed policy runs (1h) | `sum(increase(rbh_policy_runs_total{outcome="failed"}[1h]))` |
| Run rate by outcome | `sum by (outcome) (rate(rbh_policy_runs_total{policy_id=~"$policy_id"}[5m]))` |
| Action outcomes | `sum by (outcome) (rate(rbh_actions_total[5m]))` |
| Run duration p50 / p95 | `histogram_quantile(0.95, sum by (le) (rate(rbh_policy_run_duration_seconds_bucket[5m])))` |
| Changelog rate | `sum by (mdt, event_type) (rate(rbh_changelog_events_total[1m]))` |

Template variables `$policy_id` and `$mdt` come from label values.

## Alerting suggestions

Not bundled in the JSON — configure per deployment. Useful alert rules:

```
# Policy runs failing above threshold
sum by (policy_id) (increase(rbh_policy_runs_total{outcome="failed"}[15m])) > 3

# Threshold repeatedly fires but runs don't drain (no decrease in
# rbh_catalog_entries over 30m after a fire)
increase(rbh_threshold_fires_total[30m]) > 0
  and deriv(rbh_catalog_entries[30m]) == 0

# Changelog stalled for > 5 min (no events on a previously active MDT)
rate(rbh_changelog_events_total[5m]) == 0
```
