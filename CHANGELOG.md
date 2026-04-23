# Changelog

All notable changes to this project are recorded here. The project
follows [Semantic Versioning](https://semver.org/), but is still
pre-1.0 — breaking changes are expected.

## [Unreleased]

### Added

#### HSM state tracking
- `apply_event` now handles `ChangelogEvent::Hsm`: decodes
  `hsm_event` / `hsm_flags` / `hsm_error` per `<lustre/lustre_user.h>`
  and patches `entries.sm_status` with `hsm_state`, `hsm_last_op`,
  `hsm_dirty`, and `hsm_last_error` fields.
- `Predicate::HsmStateEq { state }` pushes down to a
  `JSON_UNQUOTE(JSON_EXTRACT(sm_status, '$.hsm_state')) = ?` query.
- `rbh find --hsm-state archived|released|none` CLI.
- `EntryStore::patch_sm_status(fid, patch)` merges a JSON object into
  the row's `sm_status` without clobbering existing keys.

#### P0 — operational foundations
- **Multi-MDT changelog ingest** with per-MDT cursors and error
  isolation (`RBH_MDTS=mdt0,mdt1` + `RBH_CHANGELOG_USER=cl1` or
  per-MDT list).
- **SIGHUP hot reload** of the `tracing-subscriber` filter via
  `RBH_LOG`; **SIGUSR1** dump hook; **SIGTERM/SIGINT** graceful
  shutdown with axum `with_graceful_shutdown` and changelog drain.
- **systemd units** — `rbh-daemon.service` + `rbh-daemon@.service` —
  plus sysconfig template, tmpfiles.d, and logrotate fragments.
- **Dynamic threshold triggers** — `TriggerSpec::ThresholdCount` and
  `ThresholdVolume`; polled every `RBH_THRESHOLD_TICK_SECS` and fired
  via an immediate scheduler-rs schedule with
  `rbh_observability::metrics::THRESHOLD_FIRES` counter.
- **OST / pool / user / group targeted runs** —
  `TargetFilter { Fs / Ost / Pool / User / Group / Projid }` serialized
  into `PolicyRunTask` and injected by thresholds.
- **`ignore_fileclass`** implemented end-to-end —
  `PolicyDef.ignore_fileclass: Vec<FileClassDef>` compiled into a
  `WHERE scope AND NOT (…)` prefix on the candidate query.
- **`Predicate::OnOst`** — generates `EXISTS` JOIN against
  `stripe_items`.
- **LRU-sorted candidates** via `ActionParams.lru_sort`.
- **`/api/entries/query`** — generic POST body:
  `{ predicate, order_by, limit, offset, with_total }`.
- **`rbh find`** — POSIX-style filters (`--user --group --projid
  --pool --type --name --size --mtime --atime --ctime --sort
  --desc --limit --json`) with find(1)-compatible `[+-]N[KMGTsmhdy]`
  grammar.
- **`rbh report`** — `fs-info`, `top-size`, `top-users`, `top-groups`,
  `oldest`, `dump`, `size-profile` subcommands backed by new
  `/api/reports/*` endpoints.

#### P1 — policy runtime
- **Concurrent workers** — `ActionParams.nb_threads` fans out via
  `JoinSet` + `Semaphore`. Cancel token cleanly stops new dispatch;
  in-flight workers drain.
- **Per-action rate limits** — `ActionParams.rate_limit =
  RateLimit { max_per_sec, max_bytes_per_sec }`; leaky-bucket impl
  allows oversized entries (> per-second cap) to push the bucket
  negative instead of starving.
- **Low-watermark stop** — threshold runs spawn a background monitor
  that re-counts the candidate set and cancels the run once the
  measure drops to `low_count` / `low_bytes`.
- **Per-entry `timeout_secs`** wraps each executor call in
  `tokio::time::timeout`; **`max_volume`** enforced at dispatch as a
  run-level byte budget.
- **`evaluate_rules`** reconnected — first-match rule chooses the
  effective `ActionParams` per entry.
- **HSM `archive_id` + `hints`** — `ActionParams.hsm =
  HsmParams { archive_id, hints }`, threaded into
  `HsmArchiveExecutor` and attached via `HsmRequestBuilder::data()`.
- **Retry with backoff** — `ActionParams.retry =
  RetryParams { max_attempts, backoff_secs }`; terminal outcomes
  (Success / Skipped) short-circuit the loop.

#### P2 — visibility, reporting, tooling
- **Prometheus `/api/metrics`** — six metric families:
  `rbh_catalog_entries`,
  `rbh_policy_runs_total{policy_id, outcome}`,
  `rbh_policy_run_duration_seconds{policy_id}`,
  `rbh_threshold_fires_total{policy_id}`,
  `rbh_changelog_events_total{mdt, event_type}`,
  `rbh_actions_total{policy_id, outcome}`.
- **Grafana dashboard** — `packaging/grafana/robinhood-rs.json`
  (8 panels, Prometheus datasource variable).
- **`rbh undelete list` / `forget <fid>`** — REST
  `GET /api/removed` + `DELETE /api/removed/{fid}`.
- **`rbh diff --mount PATH`** — walks a mount and diffs names
  against `/api/entries/query` paginated pulls.
- **`rbh-config-import` binary** — converts
  `robinhood-C *.conf` files to `PolicyDef` JSON (recognises
  `define_policy`, `FileClass`, trigger blocks, and basic
  expression atoms; `--post URL` optionally publishes to a daemon).
- **Incremental scan** —
  `ScanConfig.since_mtime` + `ignore_globs` + `.rbh_ignore` loader.
  REST `POST /api/scans` returns 202 + `scan_id`;
  `GET /api/scans[/id]` reports live progress.
  `rbh scan --root --since-mtime --ignore GLOB --detach`.
- **Manual policy run** — `POST /api/policies/{id}/run { target? }`
  and `rbh policy-run <id> [--target-ost N | --target-pool NAME |
  --target-user UID]`. Manual / threshold one-shot schedules are
  pruned (at startup + every 10 min) once marked `Completed`.
- **docker-compose dev stack** — MariaDB + rbh-daemon (stub
  `liblustreapi` in the image) + Prometheus + Grafana with dashboard
  provisioned.

#### Infrastructure
- **`tests/e2e.sh`** — 18-assertion shell harness: fresh DB,
  daemon launch, seed data, all major CLI + metric checks, graceful
  shutdown.

### Fixed
- `rbh find --sort size` direction: `--desc` flag instead of
  `--asc true/false` (clap-derive couldn't parse the latter cleanly).
- `rbh report fs-info` now prints `file` / `dir` / `symlink` labels
  instead of the raw `kind` discriminant.
- `parent_fid` aggregate hex-encoded (was raw BINARY(16) bytes in
  JSON).
- Manual / threshold one-shot schedule names use a short UUID suffix
  so same-second fires don't collide in logs.
- `ScanRecord.ignore_globs` now reflects globs merged from
  `.rbh_ignore` (previously only the request's globs).
- Policy run metrics fire on the "kind not implemented" early-return
  path too, so `rbh_policy_runs_total{outcome="unimplemented"}`
  accounts for alert / migration placeholder executors.
