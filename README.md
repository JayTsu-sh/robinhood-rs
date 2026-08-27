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

Catalog commands require `--filesystem ID` (or `RBH_FILESYSTEM=ID`); there is
no implicit global catalog scope.

Path lookup and stat use the filesystem-scoped `rbh-namespace` interface.
Lustre adapters keep native `path2fid`/`fid2path` behavior; JuiceFS adapters
walk the cataloged parent/name graph and verify the resulting inode through the
configured mount. An adapter rejects keys and paths from another filesystem,
and reports missing parents and stale catalog paths as distinct typed errors.

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
| `status --filesystem ID` | `/api/entries/count?filesystem=ID` |
| `health`        | `/api/health` |

## REST endpoints

| Method | Path | Notes |
|--------|------|-------|
| GET    | `/api/health`        | Liveness |
| GET    | `/api/metrics`       | Prometheus exposition |
| GET    | `/api/entries/count?filesystem=ID` | Filesystem-scoped catalog row count |
| POST   | `/api/entries/query` | Generic predicate query; body requires `filesystem` |
| POST   | `/api/reports/aggregate` | Group-by counts; body requires `filesystem` |
| GET    | `/api/reports/top-size?filesystem=ID` | Filesystem-scoped Top-N by size |
| GET    | `/api/reports/oldest?filesystem=ID` | Oldest-N by atime |
| GET    | `/api/reports/size-profile?filesystem=ID` | Bucketed size histogram |
| GET    | `/api/removed?filesystem=ID` | Filesystem-scoped removed objects |
| DELETE | `/api/removed/{object}?filesystem=ID` | Forget one typed backend object |
| GET / POST | `/api/policies`  | CRUD |
| GET / PUT / DELETE | `/api/policies/{id}` | CRUD |
| POST   | `/api/policies/{id}/run` | Manual one-shot run |
| POST   | `/api/scans`         | Start an async scan |
| GET    | `/api/scans[/{id}]`  | Scan progress |

The unscoped Lustre dump, restore, orphan-sweep, and classifier-run surfaces
are retained only under `/api/compat/lustre/` as an explicit migration boundary.

## Configuration

All runtime knobs are environment variables (hot-reloadable subset via
SIGHUP):

| Variable | Effect |
|----------|--------|
| `RBH_DATABASE_URL` | MariaDB connection string |
| `RBH_LUSTRE_MOUNT` | Legacy single-Lustre mount; translated to a filesystem runtime |
| `RBH_FILESYSTEM_ID` | Stable id for the legacy single-Lustre runtime; default `lustre` |
| `RBH_FILESYSTEMS_JSON` | Explicit filesystem registry as a JSON array; supports any mix of Lustre and JuiceFS runtimes |
| `RBH_MDTS`         | Legacy single-Lustre MDT list (empty = no changelog) |
| `RBH_CHANGELOG_USER` | Legacy single-Lustre reader id (`cl1` etc.); CSV for per-MDT |
| `RBH_LISTEN_ADDR`  | REST bind, default `0.0.0.0:8080` |
| `RBH_LOG`          | `tracing-subscriber` env-filter; **hot-reload via SIGHUP** |
| `RBH_THRESHOLD_TICK_SECS` | Threshold poll cadence, default 30 |
| `RBH_OTLP_ENDPOINT` | (Reserved; not yet wired) |

For explicit runtimes, `RBH_FILESYSTEMS_JSON` contains filesystem configuration
plus its change-source configuration. Capabilities are declared per filesystem, so a
Lustre runtime with `"hsm": false` never starts the HSM poller even when
`RBH_HSM_POLL_SECS` is non-zero. Each Lustre runtime declares its own
`lustre_changelog` array of `{ "mdt", "reader_id" }` objects; JuiceFS runtimes
declare `changelog_agent`. A failed source is restarted independently.

`RBH_LUSTRE_MOUNT`, `RBH_FILESYSTEM_ID`, `RBH_MDTS`, and
`RBH_CHANGELOG_USER` remain only as external configuration compatibility for a
deployment that does not set `RBH_FILESYSTEMS_JSON`; they are translated into
one fully scoped Lustre runtime. Likewise, global-FID catalog operations remain
reachable only through the explicitly named `/api/compat/lustre/` migration
surface. Internal scans, ingestion, classifiers, reports, policies, namespace
lookups, and actions use filesystem-scoped native identities.

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
