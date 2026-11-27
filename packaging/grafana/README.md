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
| Changelog rate | `sum by (source, event_type) (rate(rbh_changelog_events_total[1m]))` |

The `$mdt` filter uses the `source` label. Query Rev.8 policy runs and file actions through the REST history endpoints.

## Alerting suggestions

Not bundled in the JSON — configure per deployment. Useful alert rules:

```
# Changelog stalled for > 5 min (no events on a previously active MDT)
rate(rbh_changelog_events_total[5m]) == 0
```
