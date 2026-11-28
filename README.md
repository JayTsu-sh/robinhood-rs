# robinhood-rs

robinhood-rs is a Rust rewrite of the [Robinhood Policy Engine](https://github.com/cea-hpc/robinhood).
It maintains a filesystem catalog from scans and changelogs. Use the `rbh` CLI
or REST API to query the catalog, and use the web console to configure, preview,
and monitor online tiering between OST pools on the same Lustre filesystem.
Native Lustre HSM actions are available through the policy API. The cross-storage
HSM card in the console is marked **暂未开放**.

## Deploy a container that mounts Lustre

Use this deployment to connect real Lustre clusters, manage their mounts, and
save tiering policies in the console. Use rootful Docker Engine and Compose on
a Linux x86-64 Lustre client node.
Install and load the host's Lustre and LNet kernel modules, configure LNet,
verify that `/dev/lnet` exists, and check MGS connectivity from the host with
`lctl ping <MGS-NID>`. The image supplies
the Lustre client tools and shared libraries. When you add a cluster and leave
the mount path blank, the daemon mounts it at `/mnt/<filesystem-id>` inside
the container. Previously registered mount paths stay in use after an upgrade.
[Lustre client setup](https://wiki.lustre.org/Mounting_a_Lustre_File_System_on_Client_Nodes)

1. Initialize a database as described in [Initialize MariaDB](#2-initialize-mariadb).
   Use a MariaDB URL reachable from the host network.
2. Create a dedicated container configuration directory. For actual migration,
   place `principal.json` in that directory and set
   `RBH_HSM_PRINCIPAL_FILE=/run/rbh-config/principal.json`.
3. Supply the database URL, a stable observer ID, the console's private address,
   and the CIDR of the trusted operator network. Clients in that network can
   use the HTTP console and write without a token. Use the same observer ID and
   database when recreating the container so the daemon can restore saved mounts.
   The container starts with an empty filesystem registry unless you set
   `RBH_FILESYSTEMS_JSON`.

```sh
export RBH_DATABASE_URL='mysql://rbh_service:PASSWORD@127.0.0.1:3306/rbh_entries'
export RBH_CONTAINER_CONFIG_DIR=/etc/robinhood/container
export RBH_CONSOLE_OBSERVER_ID='tiering-node-1'
export RBH_CONSOLE_HTTP_LISTEN=10.0.0.10:8080
export RBH_CONSOLE_HTTP_HOST=10.0.0.10
export RBH_CONSOLE_TRUSTED_CIDR=10.0.0.0/24
export RBH_LISTEN_ADDR=127.0.0.1:18080
sudo install -d -m 0700 "$RBH_CONTAINER_CONFIG_DIR"
sudo sh -c 'umask 077; set -C; openssl rand -hex 32 > "$1/console-gateway.token"' sh "$RBH_CONTAINER_CONFIG_DIR"
docker compose -f packaging/docker/compose.yml -f packaging/docker/compose.console.yml up -d --build
curl -fsS --retry 10 --retry-connrefused http://10.0.0.10:8080/api/health
```

4. Open `http://<console-host>:8080/console/`. Under **存储资源 → Lustre 集群**, choose
   **添加集群**. Enter the Lustre filesystem name and MGS NIDs, test the connection,
   and submit to mount it. Keep **daemon 重启后恢复挂载** enabled to restore the
   mount after a restart. The restored mount appears under the same cluster entry.
   Use **卸载** in the cluster details after associated scans and operations finish.
   The cluster stays in the list and can be mounted again from its details.

The Compose service uses the host network and `/dev/lnet` and grants
`SYS_ADMIN` for mounts, `NET_ADMIN` for LNet connectivity checks, and
`DAC_READ_SEARCH` for the FID-to-path verification used by directory-scoped
policies on mounts with `nouser_fid2path`. Its AppArmor setting defaults to
`unconfined` to permit the mount. On hosts with a
profile that permits Lustre mounts and the required device access, set
`RBH_CONTAINER_APPARMOR_PROFILE` to the installed profile name. The default
HTTP listener is on loopback. The optional `RBH_CONTAINER_IMAGE` selects the
image tag, which defaults to `robinhood-rs:lustre`.
For a standalone daemon with bearer-token writes, run only
`packaging/docker/compose.yml` and install `writers.json` in the container
configuration directory. [Writer setup](#3-configure-and-start-the-daemon)
[HTTP gateway settings](packaging/README.md#build-and-serve-the-web-console)

## Develop and test without a Lustre cluster

From the repository root, start the development stack with Docker Compose and
BuildKit. The image includes the compiled console, and MariaDB imports the
schema when its volume is first created.

```sh
docker compose -f dev/compose/docker-compose.yml up -d --build
curl -fsS --retry 10 --retry-connrefused http://127.0.0.1:8080/api/health
docker compose -f dev/compose/docker-compose.yml exec rbh-daemon rbh --filesystem lustre status
```

Open [the console](http://127.0.0.1:8080/console/). To seed sample data and
preview a policy, follow the [development Compose guide](dev/compose/README.md).

## Deploy on a Lustre client

### 1. Build the daemon, CLI, and console

Run these commands on a Linux build host with Rust and Cargo, a C toolchain,
Clang/libclang for bindgen, Lustre client headers and `liblustreapi`, and Node.js
22 with npm. The runtime host needs the matching Lustre client tools and shared
libraries. Use a Lustre mount visible to the daemon and readable by its service
account.

```sh
cargo build --locked --release -p robinhood-rs -p rbh-cli
npm --prefix web ci
npm --prefix web run build
```

With Cargo's default target directory, the binaries are
`target/release/robinhood` and `target/release/rbh`. The console build is in
`web/dist/`.

### 2. Initialize MariaDB

As a database administrator, import the current schema into a new, empty
database. Use a dedicated database account for the daemon with
`SELECT`, `INSERT`, `UPDATE`, and `DELETE` on that database. For an existing
development database with a different schema version, create a separate empty
database and point the daemon at it.

```sh
mariadb --execute 'CREATE DATABASE rbh_entries'
mariadb rbh_entries < crates/rbh-entry-store/schema.sql
```

### 3. Configure and start the daemon

For console writes, configure a writer before starting the daemon. The
`RBH_POLICY_WRITERS_FILE` JSON file maps writer names to SHA-256 digests.
Generate a local test credential in the shell that will start the foreground
daemon. Use the contents of `token` as the operator's bearer token.

```sh
umask 077
writer_dir=$(mktemp -d)
openssl rand -hex 32 | tr -d '\n' > "$writer_dir/token"
digest=$(sha256sum "$writer_dir/token" | cut -d ' ' -f 1)
printf '{"operator":"%s"}\n' "$digest" > "$writer_dir/writers.json"
export RBH_POLICY_WRITERS_FILE="$writer_dir/writers.json"
```

For a service deployment, install the binaries, console, and systemd unit
using the [packaging instructions](packaging/README.md#first-time-install).
Put the database URL, listener, filesystem registry, and absolute writer-file
path in the service environment file. Give the service account read access to
the writer file. The packaged service serves the console from
`/usr/share/robinhood/console` by default. To try the same deployment in the
foreground from the repository checkout, run the following after replacing the
database credentials and mount path.

```sh
export RBH_DATABASE_URL='mysql://rbh_service:PASSWORD@127.0.0.1:3306/rbh_entries'
export RBH_LISTEN_ADDR='127.0.0.1:8080'
export RBH_CONSOLE_DIR="$PWD/web/dist"
export RBH_FILESYSTEMS_JSON='[{"id":"lustre","backend":"lustre","mount_path":"/mnt/lustre","capabilities":{"changelog":false,"namespace":true,"purge":true,"hsm":true,"stripe":true,"ost":true}}]'
./target/release/robinhood
```

An empty catalog triggers a baseline scan. To keep it current from Lustre
events, register a changelog reader per MDT on the MDS. Set the filesystem's
`capabilities.changelog` to `true` and add its `lustre_changelog` entries
with the actual `mdt` and `reader_id` values. Include `LYOUT` in the changelog
mask for layout refresh. [Filesystem configuration](#configuration)

Use loopback for local access. For remote operators, serve `/console/` and
`/api/` on the same HTTPS origin through a reverse proxy.
[Console deployment](packaging/README.md#build-and-serve-the-web-console)

From a second shell, check health and inspect the catalog. The CLI's
`--filesystem` flag takes the configured ID, such as `lustre`. For a remote
daemon, set `RBH_API_URL` to its API origin.

```sh
curl -fsS http://127.0.0.1:8080/api/health
curl -fsS http://127.0.0.1:8080/api/filesystems
./target/release/rbh --filesystem lustre status
./target/release/rbh --filesystem lustre find --size +1M --sort size --desc
./target/release/rbh --filesystem lustre report fs-info
```

To index a test subtree on demand, run
`./target/release/rbh --filesystem lustre scan --root /mnt/lustre/example`.
Use a directory that exists under the registered mount.

For daemon-managed mounts, set `RBH_LUSTRE_MANAGED_ROOT` and give the service
mount privileges. The console can then create mounts, unmount them, and restore
them after a restart when configured to do so.
[Managed mounts](packaging/README.md#daemon-managed-lustre-mounts)

## Run a tiering policy from the console

1. Open `/console/` and go to **Lustre 集群**. Select a filesystem configured at
   startup or use **添加集群** to create a new mount when the daemon manages mounts.
   Check that the source and target OST pools are both eligible, and allow the
   baseline scan to populate the catalog.
2. In **分层策略**, create a same-filesystem tiering policy. Select the filesystem
   and pool pair, choose a scan scope and file rules, and set the trigger and
   per-run limits. For a first run, use a small test directory and a manual
   trigger. Directory scope selects candidate files. The directory's default
   Pool layout stays as it is.
3. Keep **仅评估（Dry-run）** and select **计算预览**. Check the candidate counts and
   sample files before saving. New policies are saved disabled. Supply the
   writer token when prompted.
4. To move data, configure the daemon workload identity and governance below.
   Edit the policy, select **实际执行**, save, and use **启用策略**. Choose
   **立即运行** for the manual trigger. If the policy requires operator approval,
   approve the pending run in its details.
5. Open the run details to check progress, per-file dispositions and live
   target-pool evidence, or to download a report. Use the run controls to
   pause, resume, or cancel further dispatch.

For actual migration, set `RBH_HSM_PRINCIPAL_FILE` to a daemon-readable JSON
file such as `{"serviceAccount":"tiering-daemon"}` and restart the daemon after
changing its environment. Bind an authorization policy that allows the
service account, an approval policy that permits the action, and a matching
system safety policy. Select the named authorization and approval resources
in the wizard. The built-in preview governance is for Dry-run.
[Policy resource API](#policy-resources)

For a test filesystem registered as `lustre`, the following commands use
`curl` and `jq` to create the three resources for `tiering-daemon`. Set
`RBH_API_URL` to the daemon's loopback HTTP origin and
`RBH_POLICY_TOKEN_FILE` to the writer token file. For the trusted gateway
deployment above, use `http://127.0.0.1:18080` and the protected
`console-gateway.token` file. Use `tiering-local` for the authorization and
approval selections in the wizard. The system safety policy applies by
filesystem scope.

```bash
create_tiering_resource() {
  jq -c --arg kind "$2" '{apiVersion:"hsm.storage.io/v1alpha3",kind:$kind,metadata:{name:"tiering-local"},spec:.}' |
    curl -fsS "${RBH_API_URL:-http://127.0.0.1:8080}/api/$1" \
      -H "Authorization: Bearer $(cat "$RBH_POLICY_TOKEN_FILE")" \
      -H 'Content-Type: application/json' --data-binary @-
}

create_tiering_resource authorization-policies AuthorizationPolicy <<'JSON'
{"defaultDecision":"deny","statements":[{"id":"tiering","decision":"allow","actions":["lustre.pool_migrate"],"subjects":{"serviceAccounts":["tiering-daemon"]},"scope":{"filesystems":["lustre"],"objectTypes":["file"]}}]}
JSON
create_tiering_resource approval-policies ApprovalPolicy <<'JSON'
{"defaultDecision":"deny","requirements":[{"id":"tiering","actions":["lustre.pool_migrate"],"mode":"not_required"}],"floorDefaults":{"minApprovers":1,"approverRoles":["approver"]}}
JSON
create_tiering_resource system-safety-policies SystemSafetyPolicy <<'JSON'
{"scope":{"filesystems":["lustre"]},"allowedActions":["lustre.pool_migrate"]}
JSON
```

Provide the executor with write access to the mount and live FID-to-path
lookup for directory-scoped policies.
For parallel migration, set the policy's same-filesystem concurrency and
choose an execution profile with enough global slots.
[Systemd and executor permissions](packaging/README.md#enable-pool-migration-under-systemd)
· [Pool migration API](#lustre-pool-migration)

You can review or start a saved policy from the CLI as well. Replace `1`
with the policy ID and set `RBH_POLICY_TOKEN_FILE` to the writer's token file
for a manual run.

```sh
./target/release/rbh policy-list
./target/release/rbh policy-show 1
./target/release/rbh policy-run 1 --dry-run
RBH_POLICY_TOKEN_FILE=/path/to/token ./target/release/rbh policy-run 1
```

## Architecture

```
              ┌────────────────────────┐
              │  robinhood (daemon)    │
              │ ─────────────────────  │
              │ signals.rs   SIGHUP    │
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
                     │ rbh-policy    │ durable policy runs
                     │               │ → Lustre HSM and pool migration
                     └───────┬───────┘
                             │
                             ▼
                     ┌───────────────┐
                     │ /api/* (axum) │◀── rbh-cli (find / report / ...)
                     │ /api/metrics  │◀── Prometheus
                     └───────────────┘
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
| `policy-run <id>` | Queue a policy run with `--token-file PATH` or `RBH_POLICY_TOKEN_FILE`. Use `--dry-run` to preview. |
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
| DELETE | `/api/policies/{id}` | Stop scheduling and retire the policy |
| POST   | `/api/policies/{id}/run` | Manual one-shot run |
| POST   | `/api/policies/{id}/preview` | Read-only candidate totals |
| POST   | `/api/scans`         | Start an async scan |
| GET    | `/api/scans[/{id}]`  | Scan progress |

The unscoped Lustre dump, restore, orphan-sweep, and classifier-run surfaces
are retained only under `/api/compat/lustre/` as an explicit migration boundary.

### Database and policy configuration

Initialize a new MariaDB database with [schema.sql](crates/rbh-entry-store/schema.sql).
Compose imports it when its database volume is created. The daemon checks the
schema version at startup. For an incompatible development database, create
a separate empty database and point `RBH_DATABASE_URL` at it.

Submit policies through REST as `hsm.storage.io/v1alpha3` documents with
`kind: Policy`, `metadata` and `spec`. Unsupported formats return HTTP 400.

### Native Lustre HSM

The trusted-network container deployment accepts policy writes through its HTTP
gateway. For a bearer-token deployment, set `RBH_POLICY_WRITERS_FILE` to a
server-side JSON map from writer principal names to the hex SHA-256 digests of
their bearer tokens. Send a configured token in the `Authorization: Bearer …`
header over a local connection or through a TLS-terminating proxy. The daemon
records the writer principal from its configured allowlist.

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

To enable execution, point `RBH_HSM_PRINCIPAL_FILE` to a JSON file supplied
by the daemon deployment, for example
`{"serviceAccount":"hsm-controller","roles":["storage-operator"]}`. The
daemon uses this identity for both scheduled and manual native submissions.
The API writer identity is recorded separately. The built-in submit path
requires MariaDB `innodb_flush_log_at_trx_commit=1` and a durable database
volume. Deploy the daemon as the trusted actuator. Separate deployment
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

### Policy execution history

- `GET /api/runs` lists runs newest first. Filters include `policyId`,
  `policyName` (a substring of the name saved with the run), `runId`, and
  `state`.
- `GET /api/runs/{id}` includes the run's pinned revision and
  `hasPendingOperations`. A `finished` run may still have operations in
  `submitting`, `submitted`, or `unknown`.
- Run list and detail responses include `operationResults` for all recorded
  operations in each run. `completedLogicalBytes` is an exact decimal string
  summed from completed operations. `estimatedBytesExact` is the candidate
  estimate. `directoryScope`, when present, contains the directory exclusion
  and deferral counts from the initial candidate evaluation.
- `GET /api/runs/{id}/operations` lists recorded operations oldest first.
  Filter by `state`, `reason` (a literal substring), or full Lustre
  `fid`. Use `exceptionsOnly=true` for failed, deferred, and unknown
  operations. Migration plans and observed layout evidence are separate fields.
- `GET /api/runs/{id}/operations/summary` applies the operation filters to the
  full run and returns totals and exception counts by recorded state and reason.
- `GET /api/runs/{id}/reports/run` and `GET /api/runs/{id}/reports/exceptions`
  download versioned JSON with the effective filters, the pinned policy
  revision, totals, and every matching operation. These routes use the same
  operation filters and ignore page cursors and limits. Exception reports
  include failed, deferred, and unknown operations. Each download uses one
  read snapshot for its run, totals, and operations. The server prepares up to
  two reports concurrently and returns HTTP 503 when both slots are in use.
  It ends the stream at the five-minute deadline or when buffered output
  waits 30 seconds for the client.
- Each list returns `{items, nextCursor}`. Use the cursor as `beforeId`
  for runs or `afterId` for operations with the same filters. A null
  cursor means the last page. The default `limit` is 25 and the maximum
  is 100. Cursors and the `*Exact` fields carry IDs and byte values as
  decimal strings for JavaScript clients.
- Add `includeLatestRun=true` to the current policy list or detail request
  to include the dispatching run, the latest queued run, or the latest run
  in that order of preference.

### Policy resources

Manage `Selector`, `ExecutionProfile`, `AuthorizationPolicy`, `ApprovalPolicy`
and `SystemSafetyPolicy` at `/api/selectors`, `/api/execution-profiles`,
`/api/authorization-policies`, `/api/approval-policies` and
`/api/system-safety-policies`. Submit a resource document with `POST` on the
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

### Lustre pool migration

Register the Lustre mount with HSM, stripe and OST capabilities. A pool
migration policy selects one pool pair on that filesystem. It supports periodic,
manual, and source-pool capacity triggers.
The action contract is available at `GET /api/action-contracts/lustre.pool_migrate`.
When using the packaged systemd unit, allow writes to the executor mount.
[Configure the service drop-in](packaging/README.md#enable-pool-migration-under-systemd).
For example, use these fields in a policy:

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
to a positive duration such as `24h` to require time in the source
pool. The catalog starts the clock on its first verified observation of
the pool or a change of pool, and retains it across rescans and restarts.
The first release handles online regular files with an initialized
single-component RAID0 layout.

The blocking copy checks the deadline between I/O calls and rechecks the
admission window before swapping layouts. If an I/O call outlasts the run
budget, the worker records an uncertain result and resumes ticking. The
blocking call retains its filesystem slot until it exits and can be reconciled.
A completed operation includes the actual pool and OSTs from a fresh open by
FID. If the catalog write fails, the pending operation retains the slot and
the restarted daemon reconciles the live layout before any new migration.

### Lustre read heat

Add a `read_heat` block to a Lustre filesystem in `RBH_FILESYSTEMS_JSON`.
A sampler starts for that filesystem when an enabled pool-migration
policy depends on read heat, and stops when the last such policy is disabled
or deleted. Each client entry names a business Lustre mount or a heat agent
endpoint.
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

Submit a separate periodic policy through the policy API for warming.
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

Set daemon variables in the launching shell or the systemd environment file
(`/etc/sysconfig/rbh-daemon` or `/etc/default/rbh-daemon`). Restart the daemon
after editing the service environment file. `SIGHUP` reloads the log filter
from the running process environment.

### Daemon environment variables

| Variable | When to set it and default |
| --- | --- |
| `RBH_DATABASE_URL` | Set to the deployment's MariaDB connection URL. The local fallback is `mysql://root@127.0.0.1/rbh_entries`. |
| `RBH_LISTEN_ADDR` | HTTP bind address for both REST and the console. Defaults to `0.0.0.0:8080`. Use `127.0.0.1:8080` behind a local reverse proxy. |
| `RBH_FILESYSTEMS_JSON` | JSON array of Lustre and JuiceFS runtimes. When omitted, the daemon derives a single Lustre runtime from the legacy variables below. |
| `RBH_LUSTRE_MOUNT` | Mount path for the legacy runtime. Defaults to `/lustre`. |
| `RBH_FILESYSTEM_ID` | Stable identifier for the legacy runtime. Defaults to `lustre`. |
| `RBH_MDTS` | Comma-separated MDT names for the legacy runtime. `RBH_MDT_NAME` is the single-MDT fallback. |
| `RBH_CHANGELOG_USER` | Legacy changelog reader ID, such as `cl1`, or one ID per MDT in the same order. Changelog ingestion requires both an MDT and a reader ID. |
| `RBH_CONSOLE_DIR` | Absolute path to the built console assets. Defaults to `/usr/share/robinhood/console`. Set it to the absolute `web/dist` path for a checkout build. |
| `RBH_CONSOLE_COLLECT_SECS` | Lustre resource collection interval in seconds. Defaults to `60` and accepts positive integers. |
| `RBH_CONSOLE_OBSERVER_ID` | Resource snapshot and console registration scope. Defaults to the hostname. Set a stable, unique value when multiple daemons with the same hostname share MariaDB. |
| `RBH_LUSTRE_MANAGED_ROOT` | Set for daemon-created Lustre mounts. Use an existing canonical directory owned by the daemon account. The daemon also needs host mount privileges. |
| `RBH_POLICY_WRITERS_FILE` | Enables bearer-token writes. The daemon reads a JSON map of writer names to SHA-256 token digests. |
| `RBH_TRUSTED_GATEWAY_WRITES` | Defaults to `false`. Set to `true` with a loopback daemon listener and an access-controlled gateway. Leave `RBH_POLICY_WRITERS_FILE` unset in this mode. |
| `RBH_TRUSTED_GATEWAY_TOKEN_FILE` | In trusted gateway mode, the daemon reads a 64-digit hexadecimal token from this file. The gateway supplies the credential on forwarded requests. |
| `RBH_TRUSTED_GATEWAY_SOCKET` | In trusted gateway mode, an absolute Unix socket path inside a directory shared exclusively with the gateway. The daemon also keeps its loopback API listener. |
| `RBH_HSM_PRINCIPAL_FILE` | Set for actual policy execution. The daemon reads a JSON workload identity with a `serviceAccount` authorized for the requested action. |
| `RBH_HSM_POLL_SECS` | Native HSM state polling interval in seconds for HSM-capable runtimes. Defaults to `0`, which disables the poller. |
| `RBH_HSM_POLL_BATCH` | Native HSM state polling batch size. Defaults to `200`. |
| `RBH_HSM_POLL_PAUSE_MS` | Delay between native HSM polling batches in milliseconds. Defaults to `500`. |
| `RBH_LOG` | Log filter. Falls back to `RUST_LOG`, then `info`. |

### CLI environment variables

| Variable | When to set it and default |
| --- | --- |
| `RBH_API_URL` | Daemon API origin for `rbh`. Defaults to `http://127.0.0.1:8080`. |
| `RBH_FILESYSTEM` | Filesystem ID for catalog commands. Use `--filesystem ID` to select it per command. |
| `RBH_POLICY_TOKEN_FILE` | File containing the writer's bearer token for `rbh policy-run`. `--token-file PATH` also works. |

Pool migration concurrency is configured by `maxConcurrency` on the policy
action and `concurrency.global` on its execution profile. For read-heat agent
variables, see [Lustre read heat](#lustre-read-heat). For test fixtures, see the
[integration test guide](tests/README.md).

For explicit runtimes, `RBH_FILESYSTEMS_JSON` contains filesystem configuration
plus its change-source configuration. Capabilities are declared per filesystem, so a
Lustre runtime with `"hsm": false` never starts the HSM poller even when
`RBH_HSM_POLL_SECS` is non-zero. Each Lustre runtime declares its own
`lustre_changelog` array of `{ "mdt", "reader_id" }` objects; JuiceFS runtimes
declare `changelog_agent`. A failed source is restarted independently.
For JuiceFS agent installation, network configuration, and daemon wiring, see
the [JuiceFS changelog agent guide](agents/juicefs-changelog-agent/README.md).

Include `LYOUT` in each Lustre reader's changelog mask to refresh catalog
layouts after a change. Entry responses expose the pool in `pool_name` and
concrete OST indices in `stripe_items`. A layout spanning several pools has
a null pool name and still reports its allocated OSTs.

Periodic `lustre.pool_migrate` policies can match the Lustre access timestamp
with `{"field":"object.last_access","op":"older_than","value":"2m"}`. Eligibility
is evaluated on each policy run. Scans and available `ATIME` changelog records
refresh the catalog timestamp. Before moving a candidate, the daemon refreshes
a changed live atime and defers that attempt. It also checks the timestamp under
the source lock before the layout swap. This predicate uses the atime visible
to the executor's Lustre client. The client mount policy and the filesystem's
atime propagation determine which reads update that timestamp.

A completed pool migration confirms the target layout after closing the
temporary descriptor that held the old stripes. OST usage can reflect the
space reclaim after the operation is reported complete.

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
| `rbh_changelog_events_total` | `mdt`, `event_type` | Per-MDT per-type ingest rate |

See [`packaging/grafana/robinhood-rs.json`](packaging/grafana/robinhood-rs.json)
for a ready-to-import dashboard.

Query policy runs and file-level actions through the REST history
endpoints.

## Development

```bash
# Unit tests (no Lustre needed):
cargo test --workspace --lib

# End-to-end smoke (requires a disposable local MariaDB):
cargo build --locked --release -p robinhood-rs -p rbh-cli
bash tests/e2e.sh

# Full gate:
cargo check --workspace --all-targets
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Local catalog and API stack without a Lustre mount
cd dev/compose && docker compose up -d --build
```

See the [integration test guide](tests/README.md) for smoke-test setup.

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
