# robinhood-rs

A Rust rewrite of the [Robinhood Policy Engine](https://github.com/cea-hpc/robinhood)
for Lustre filesystems. Policies are stored in a database and managed
via REST; an operator-facing `rbh` CLI wraps the REST surface.

> **Status.** Pre-1.0. The catalog, policy engine, HSM event
> tracking, threshold triggers, and operator tooling are all
> implemented; disaster-recovery, the full `backup` module, and the
> Web GUI from robinhood-C are not. See
> [`docs/C-VS-RUST-GAP-ANALYSIS.md`](docs/C-VS-RUST-GAP-ANALYSIS.md)
> for the tracked delta.

---

## Architecture

```
              ┌────────────────────────┐
              │  rbh-daemon (binary)   │
              │ ─────────────────────  │
              │ signals.rs   SIGHUP    │
              │ thresholds.rs          │
              │ changelog.rs ingest    │
              └────┬─────────┬─────────┘
                   │         │
                   ▼         ▼
    ┌────────────────┐   ┌────────────────┐       ┌───────────────┐
    │ lustre-changelog│   │ rbh-fs-scan    │──────▶│ rbh-entry-    │
    │ per-MDT listener│   │ (init+incr)    │       │ store (MariaDB)│
    └────────┬────────┘   └────────────────┘       └───────────────┘
             │                                              ▲
             ▼                                              │
    ┌────────────────┐                                      │
    │ entries.       │◀────── rbh-predicate ────────────────┘
    │ sm_status JSON │        (typed tree → SQL pushdown)
    └────────────────┘
                     ┌───────────────┐
                     │ scheduler-rs  │ time triggers
                     └───────┬───────┘
                             ▼
                     ┌───────────────┐
                     │ rbh-policy    │ PolicyRunTask
                     │ + rbh-actions │ → Purge / HSM archive/release
                     └───────┬───────┘
                             │
                             ▼
                     ┌───────────────┐
                     │ /api/* (axum) │◀── rbh-cli (find / report / ...)
                     │ /api/metrics  │◀── Prometheus
                     └───────────────┘
```

## Build

```bash
# Native (requires lustre-client for liblustreapi):
cargo build --release

# Or: use the compose stack with a stub liblustreapi:
cd dev/compose && docker compose up -d --build
```

## Quick start

```bash
# 1. Point the daemon at MariaDB and (optionally) your Lustre MDTs.
export RBH_DATABASE_URL=mysql://root@127.0.0.1/rbh_entries
export RBH_LUSTRE_MOUNT=/lustre
export RBH_MDTS=testfs-MDT0000           # unset = no changelog listener
export RBH_CHANGELOG_USER=cl1            # register via `lctl changelog_register` on the MDS

./target/release/robinhood &

# 2. The daemon runs the initial fs-scan on first boot if the catalog
# is empty, then keeps it current from the Lustre changelog.
#
# 3. Use the CLI to inspect / query / act.
./target/release/rbh find --user 0 --size +1M --sort size --desc
./target/release/rbh report fs-info
./target/release/rbh scan --root /lustre --since-mtime $(date -d '-24h' +%s)
```

## Commands

| `rbh` subcommand | Purpose |
|------------------|---------|
| `find`          | find(1)-style catalog query (`--user --type --size --mtime --hsm-state --sort` etc.) |
| `report`        | `fs-info`, `top-size`, `top-users`, `top-groups`, `oldest`, `size-profile`, `dump` |
| `scan`          | Start / poll an async fs-scan (incremental via `--since-mtime`, ignore globs, `--detach`) |
| `undelete list` | List entries in `removed_entries` |
| `undelete forget <fid>` | Drop a removed-entry row after external recovery |
| `diff --mount`  | Walk a mount and diff `(name, size)` against the catalog |
| `policy-list`   | List policies |
| `policy-show <id>` | Show one policy |
| `policy-run <id>` | Manual one-shot run, optional `--target-ost N` / `--target-pool NAME` / `--target-user UID` |
| `status`        | `/api/entries/count` |
| `health`        | `/api/health` |

## REST endpoints

| Method | Path | Notes |
|--------|------|-------|
| GET    | `/api/health`        | Liveness |
| GET    | `/api/metrics`       | Prometheus exposition |
| GET    | `/api/entries/count` | Catalog row count |
| POST   | `/api/entries/query` | Generic predicate query |
| POST   | `/api/reports/aggregate` | Group-by counts |
| GET    | `/api/reports/top-size` | Top-N by size |
| GET    | `/api/reports/oldest`    | Oldest-N by atime |
| GET    | `/api/reports/size-profile` | Bucketed size histogram |
| GET    | `/api/removed`       | `removed_entries` page |
| DELETE | `/api/removed/{fid}` | Forget a removed-entry row |
| GET / POST | `/api/policies`  | CRUD |
| GET / PUT / DELETE | `/api/policies/{id}` | CRUD |
| POST   | `/api/policies/{id}/run` | Manual one-shot run |
| POST   | `/api/scans`         | Start an async scan |
| GET    | `/api/scans[/{id}]`  | Scan progress |

## Configuration

All runtime knobs are environment variables (hot-reloadable subset via
SIGHUP):

| Variable | Effect |
|----------|--------|
| `RBH_DATABASE_URL` | MariaDB connection string |
| `RBH_LUSTRE_MOUNT` | Mount point for FID → path resolution |
| `RBH_MDTS`         | Comma-separated MDT names (empty = no changelog) |
| `RBH_CHANGELOG_USER` | Pre-registered reader id (`cl1` etc.); CSV for per-MDT |
| `RBH_LISTEN_ADDR`  | REST bind, default `0.0.0.0:8080` |
| `RBH_LOG`          | `tracing-subscriber` env-filter; **hot-reload via SIGHUP** |
| `RBH_THRESHOLD_TICK_SECS` | Threshold poll cadence, default 30 |
| `RBH_OTLP_ENDPOINT` | (Reserved; not yet wired) |

## Signals

| Signal | Effect |
|--------|--------|
| SIGTERM / SIGINT | Graceful shutdown — changelog drain + cursor commit |
| SIGHUP | Re-read `RBH_LOG`; `systemctl reload rbh-daemon` maps to this |
| SIGUSR1 | Dump stats hook |

## Metrics

| Metric | Labels | Meaning |
|--------|--------|---------|
| `rbh_catalog_entries` | — | Current catalog row count |
| `rbh_policy_runs_total` | `policy_id`, `outcome` | Policy runs completed |
| `rbh_policy_run_duration_seconds` | `policy_id` | Histogram |
| `rbh_threshold_fires_total` | `policy_id` | Threshold trigger fires |
| `rbh_changelog_events_total` | `mdt`, `event_type` | Per-MDT per-type ingest rate |
| `rbh_actions_total` | `policy_id`, `outcome` | Per-entry action outcomes |

See [`packaging/grafana/robinhood-rs.json`](packaging/grafana/robinhood-rs.json)
for a ready-to-import dashboard.

## Migrating from robinhood-C

```bash
cargo run -p rbh-config-import -- /path/to/old-robinhood.conf --pretty
# or POST directly:
cargo run -p rbh-config-import -- old.conf --post http://127.0.0.1:8080
```

The importer recognises a useful subset of the C DSL (see source for
scope); anything it can't translate is logged as a warning so the
resulting JSON is hand-editable.

## Development

```bash
# Unit tests (no Lustre needed):
cargo test --workspace --lib

# End-to-end smoke (requires MariaDB reachable as root):
bash tests/e2e.sh

# Full gate:
cargo check --workspace --all-targets
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Local stack in containers (no Lustre; stub liblustreapi):
cd dev/compose && docker compose up -d --build
```

## Packaging

* `packaging/systemd/*.service` — unit files with `ExecReload = kill -HUP`
* `packaging/sysconfig/rbh-daemon` — env template
* `packaging/tmpfiles.d/robinhood.conf` — runtime dirs
* `packaging/logrotate/rbh-daemon`
* `packaging/grafana/robinhood-rs.json` — dashboard

See [`packaging/README.md`](packaging/README.md) for install steps.

## Related projects

| Path | Role |
|------|------|
| `/root/lustre/robinhood` | C reference being rewritten |
| `/root/rust/scheduler-rs` | Policy scheduling backbone (git dep) |

## License

See [`LICENSE`](LICENSE).
