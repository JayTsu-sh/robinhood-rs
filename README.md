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
| GET    | `/api/entries/{object_id}?filesystem=ID` | Look up one Lustre FID or JuiceFS inode |
| POST   | `/api/entries/query` | Generic predicate query; body requires `filesystem` |
| POST   | `/api/reports/aggregate` | Group-by counts; body requires `filesystem` |
| GET    | `/api/reports/top-size?filesystem=ID` | Filesystem-scoped Top-N by size |
| GET    | `/api/reports/oldest?filesystem=ID` | Oldest-N by atime |
| GET    | `/api/reports/size-profile?filesystem=ID` | Bucketed size histogram |
| GET    | `/api/removed?filesystem=ID` | Filesystem-scoped removed objects |
| DELETE | `/api/removed/{object}?filesystem=ID` | Forget one typed backend object |
| GET / POST | `/api/policies`  | CRUD |
| GET / PUT | `/api/policies/{id}` | Read or update a policy |
| DELETE | `/api/policies/{id}` | Delete a legacy policy |
| POST   | `/api/policies/{id}/run` | Manual one-shot run |
| POST   | `/api/policies/{id}/preview` | Read-only Rev.8 candidate totals |
| POST   | `/api/scans`         | Start an async scan |
| GET    | `/api/scans[/{id}]`  | Scan progress |

The unscoped Lustre dump, restore, orphan-sweep, and classifier-run surfaces
are retained only under `/api/compat/lustre/` as an explicit migration boundary.

### Rev.8 native Lustre HSM

Set `RBH_REV8_WRITERS_FILE` to a server-side JSON map from writer principal
names to the hex SHA-256 digests of their high-entropy bearer tokens. Send a
configured token in the `Authorization: Bearer …` header for every Rev.8
configuration write. Use a local connection or a TLS-terminating proxy for
the token. The service records the principal from its configured allowlist.

Send the following document to `POST /api/policies` as JSON, substituting
the ID of a registered Lustre filesystem with HSM enabled.

```json
{
  "apiVersion": "hsm.storage.io/v1alpha3",
  "kind": "Policy",
  "metadata": { "name": "archive-large-files" },
  "spec": {
    "target": { "filesystem": "lustre", "objectType": "file" },
    "trigger": { "type": "manual" },
    "objective": { "type": "exhaust" },
    "rules": {
      "entries": [{
        "match": { "field": "object.size", "op": "gte", "value": "1GiB" },
        "decision": "execute",
        "action": { "type": "lustre.hsm.archive", "params": { "archiveId": 1 } }
      }]
    },
    "execution": {
      "profileRef": "builtin-preview",
      "activation": { "mode": "preview" },
      "budget": { "run": { "maxOperations": 100, "maxSubmitAttempts": 100, "maxObjectLogicalBytes": "1TiB", "maxDuration": "30m" } }
    },
    "governance": { "authorizationRef": "builtin-preview", "approvalPolicyRef": "builtin-preview" }
  }
}
```

Use the returned `id` to send `POST /api/policies/{id}/preview` with no request
body. The result gives the stored `revision`, the catalog `candidateCount`,
and the total logical `estimatedBytes` before any execution budget is applied.
`GET /api/policies/{id}` returns the stored document under `definition`.
`PUT /api/policies/{id}` saves a new revision. This preview accepts indexed
regular files, the built-in preview references, and first-match archive rules
with size comparisons or boolean conditions.

To enable execution, point `RBH_REV8_HSM_PRINCIPAL_FILE` to a JSON file supplied
by the daemon deployment, for example
`{"serviceAccount":"hsm-controller","roles":["storage-operator"]}`. The
daemon uses this identity for both scheduled and manual native submissions.
The API writer identity is recorded separately. The built-in submit path
requires MariaDB `innodb_flush_log_at_trx_commit=1` and a durable database
volume. Deploy the daemon as the trusted actuator. Rev.8's separate deployment
attestation and cross-node permit log verification are outside this execution
path.

Use `lustre.hsm.archive`, `lustre.hsm.release` or `lustre.hsm.restore`. Configure
an applicable `SystemSafetyPolicy`, grant the workload access in an
`AuthorizationPolicy`, and select an `ApprovalPolicy` that explicitly allows
unattended execution for the action. Set `spec.execution.activation.mode` to
`enforced` and bind a saved execution profile. This serial executor accepts a
single submit attempt per operation. Set `concurrency.global` to cap unresolved
operations sharing the profile. Policy-scoped consecutive-failure breakers
support definitive `backend_unavailable` and `transient_io` errors and manual
reset. System safety limits cap unresolved submissions. Authorization can constrain filesystem,
object type and archive ID. Activation rejects grants that require namespace
or OST authorization, human approval, or unsupported profile combinations.
Use the profile and policy admission windows to restrict submission times.

- `POST /api/policies/{id}/run` queues a manual run and returns its `runId`.
  A periodic trigger queues runs automatically. One-shot runs wait for available
  profile or safety capacity within their run deadline. The preview endpoint
  evaluates candidates immediately without writing a run.
- `GET /api/policies/{id}/runs` lists runs. `GET /api/policies/{id}/runs/{run}/operations`
  lists the file-level decisions. Both accept `afterId` for pagination.
- `submitted` records a successful Lustre request response. `completed` records
  matching live Lustre HSM state for the FID and expected file generation.
  A run can finish dispatch while individual requests are still outstanding.
- Set `spec.enabled` to `false` with `PUT`, or delete the policy with `DELETE`,
  to stop new submissions. Operation history remains available by policy ID
  and pending requests continue reconciling against their bound revision.
- `POST /api/policies/{id}/breaker/reset` resets the policy circuit breaker
  with an authenticated writer. Outstanding operation limits remain in force.

### Rev.8 policy resources

Manage `Selector`, `ExecutionProfile`, `AuthorizationPolicy`, `ApprovalPolicy`
and `SystemSafetyPolicy` at `/api/selectors`, `/api/execution-profiles`,
`/api/authorization-policies`, `/api/approval-policies` and
`/api/system-safety-policies`. Submit a Rev.8 document with `POST` on the
collection and use `GET`, `PUT` or `DELETE` at the resource name. The controls
registered by the server are available at `GET /api/action-contracts` and
`GET /api/action-contracts/{name}`.

Use `selectorRef` inside a supported condition and set `profileRef`,
`authorizationRef` and `approvalPolicyRef` to the names of saved resources.
Matching system safety resources are bound by scope. Each policy revision
pins the resolved resource revisions, including transitive selectors.
Matching system safety changes create a new revision of affected enabled
policies. Submit a new policy revision to adopt changes to named resources.
Preview responses
return the bindings in `resourceBindings`. Read the pinned definitions at
`GET /api/{collection}/{name}/revisions/{revision}` or find the original
policy binding at `GET /api/policies/{id}/revisions/{revision}`. References
from enabled policies and from other selectors block resource deletion.

### Rev.8 Lustre pool migration

Register the Lustre mount with HSM, stripe and OST capabilities. A pool
migration policy has a periodic trigger and one pool pair on that filesystem.
The action contract is available at `GET /api/action-contracts/lustre.pool_migrate`.
For example, use these fields in a Rev.8 policy:

```json
{
  "target": {
    "filesystem": "lustre",
    "objectType": "file",
    "where": {
      "all": [
        {"field": "lustre.pool", "op": "eq", "value": "hot"},
        {"field": "object.tags.temperature", "op": "eq", "value": "coolable"},
        {"field": "object.last_modified", "op": "older_than", "value": "7d"}
      ]
    }
  },
  "trigger": {"type": "periodic", "interval": "1h"},
  "objective": {"type": "exhaust"},
  "rules": {
    "entries": [{
      "match": true,
      "decision": "execute",
      "action": {
        "type": "lustre.pool_migrate",
        "targetRef": "current.object",
        "params": {"sourcePool": "hot", "targetPool": "cold", "maxFileBytes": "64GiB"}
      }
    }]
  }
}
```

Merge these fields into `spec` and bind the execution and governance resources
described above. For enforced execution, authorize `lustre.pool_migrate`, provide
matching system safety and approval policies, and set the run budget including
`maxDuration`. The candidate count uses indexed conditions and source pool.
Execution rechecks the source placement and HSM state. Classifier tags come
from the catalog and are rechecked on the locked candidate row along with its
indexed file generation. Optional `stripeCount`
and `stripeSize` override the source stripe geometry. Set `minResidence`
to a positive Rev.8 duration such as `24h` to require time in the source
pool. The catalog starts the clock on its first verified observation of
the pool or a change of pool, and retains it across rescans and restarts.
For existing observed layouts, the upgrade starts the clock at upgrade time.
The first release handles online regular files with an initialized
single-component RAID0 layout.

The blocking copy checks the deadline between I/O calls. An I/O call already
inside Lustre retains its filesystem slot until it exits and can be reconciled.
A completed operation includes the actual pool and OSTs from a fresh open by
FID. If the catalog write fails, the pending operation retains the slot and
the restarted daemon reconciles the live layout before any new migration.

### Rev.8 Lustre read heat

Add a `read_heat` block to a Lustre filesystem in `RBH_FILESYSTEMS_JSON`.
Each client entry names a business Lustre mount or a heat agent endpoint.
Business mounts must be separate client instances mounted from the same
Lustre source as the executor's `mount_path`. The daemon checks client
identity across hosts and rejects shared instances, including bind mounts
of an executor client. Enable `file_heat` and use matching
`heat_period_second` and `heat_decay_percentage` on the business clients.

```json
{
  "read_heat": {
    "clients": {
      "reader-a": "/mnt/business-a",
      "reader-b": {
        "endpoint": "https://business-b.example.com/sample",
        "token_file": "/etc/robinhood/reader-b.token"
      }
    },
    "decay": {"period_seconds": 60, "percentage": 80},
    "interval_seconds": 2,
    "max_age_seconds": 120,
    "batch_size": 128
  }
}
```

On a separate business host, run `robinhood heat-agent` with
`RBH_HEAT_AGENT_MOUNT` set to the mount used by the application and
`RBH_HEAT_AGENT_TOKEN_FILE` set to a private file containing the same
random bearer token as the controller's `token_file`. The agent binds to
`127.0.0.1:9184` by default. Set `RBH_HEAT_AGENT_LISTEN` to change the
loopback address or port. Expose `/sample` through an HTTPS proxy or
forward a controller loopback port to the agent over SSH. The controller
accepts plain HTTP endpoints only on loopback. Keep the controller and
agent clocks synchronized. The agent serves bounded read-only samples on
request and runs without a database connection. The sampling process needs
`CAP_DAC_READ_SEARCH` for Lustre's FID handle open. Use the standalone agent
for local business mounts too when the controller runs as the unprivileged
packaged service.

Submit a separate periodic policy through the Rev.8 policy API for warming.
For example, set this execute rule and use the execution and governance
resources for `lustre.pool_migrate` described above.

```json
{
  "match": {"field": "lustre.heat.readsample", "op": "gte", "value": 20},
  "decision": "execute",
  "action": {
    "type": "lustre.pool_migrate",
    "params": {
      "sourcePool": "cold", "targetPool": "hot",
      "maxFileBytes": "64GiB", "minResidence": "24h"
    }
  }
}
```

The supported fields are `lustre.heat.readsample` and `lustre.heat.readbyte`.
Each is the maximum decayed score for that counter across the configured
clients. Enabling enforced heat-driven migration requires a configured
`read_heat` profile and an explicit `minResidence`. Each execute decision
must depend on heat, whether through its own condition, the policy scope,
or an earlier heat-based skip rule.

The daemon samples up to `batch_size` FIDs per interval on a filesystem with
an enabled heat policy. It rotates through such policies and their eligible
FIDs. Set the sample lifetime to cover the rotation through the candidate
set. Per-client heat comes from the open inode on that client and can reset
when the client evicts the inode. The collector opens by FID and calls
`llapi_heat_get` without reading payload data. Before admitting a migration,
the worker refreshes heat for the selected FID on the same business clients.
A busy sampler or a refresh that exceeds its deadline defers the candidate.

`POST /api/policies/{id}/preview` includes `readHeat.clients`,
`readHeat.decay`, `readHeat.maxAgeSeconds`, and `readHeat.deferredByReason`.
Append `?heatFid=0x200000401:0x1a:0x0` to include the latest observation for
that FID on each configured client. Each observation includes the time,
scores, measured decay and status. The query parameter leaves the policy
totals scoped to the entire policy. All configured clients must have
fresh observations with the requested decay profile. An observed zero
participates in the comparison. Missing samples, expired samples, unavailable
clients and incompatible decay defer decisions that depend on the unavailable
value. `GET /api/policies/{id}/runs` includes `heatDeferredByReason` with the
counts from the preview or each evaluated dispatch page. A candidate deferred
during admission has its reason in `GET /api/policies/{id}/runs/{runId}/operations`.

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

Include `LYOUT` in each Lustre reader's changelog mask to refresh catalog
layouts after a change. Entry responses expose the pool in `pool_name` and
concrete OST indices in `stripe_items`. A layout spanning several pools has
a null pool name and still reports its allocated OSTs.

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
