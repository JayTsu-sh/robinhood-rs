# Packaging assets for robinhood-rs

```
systemd/
├── rbh-daemon.service      → /usr/lib/systemd/system/rbh-daemon.service
├── rbh-daemon@.service     → /usr/lib/systemd/system/rbh-daemon@.service
├── juicefs-changelog-agent.service → optional JuiceFS-side gRPC agent
└── juicefs-changelog-agent-firewall.service → source-IP allowlist for the agent
nftables/
└── juicefs-changelog-agent.nft → allows 10.131.9.10/32 to TCP 9443
sysconfig/
└── rbh-daemon              → /etc/sysconfig/rbh-daemon (RHEL/SUSE)
                               or /etc/default/rbh-daemon (Debian)
tmpfiles.d/
└── robinhood.conf          → /usr/lib/tmpfiles.d/robinhood.conf
logrotate/
└── rbh-daemon              → /etc/logrotate.d/rbh-daemon
```

## First-time install

```bash
# Create service user.
useradd --system --no-create-home --shell /sbin/nologin robinhood

# Install binaries.
install -m 0755 target/release/robinhood /usr/sbin/robinhood
install -m 0755 target/release/rbh       /usr/bin/rbh

# Install assets.
install -d -m 0755 /usr/share/robinhood/console
cp -a web/dist/. /usr/share/robinhood/console/
install -m 0644 crates/rbh-entry-store/schema.sql /usr/share/robinhood/schema.sql
install -m 0644 packaging/systemd/rbh-daemon.service  /usr/lib/systemd/system/
install -m 0644 packaging/systemd/rbh-daemon@.service /usr/lib/systemd/system/
install -m 0640 packaging/sysconfig/rbh-daemon        /etc/sysconfig/rbh-daemon
install -m 0644 packaging/tmpfiles.d/robinhood.conf   /usr/lib/tmpfiles.d/
install -m 0644 packaging/logrotate/rbh-daemon        /etc/logrotate.d/

# Initialize an empty MariaDB database as a database administrator.
mariadb --execute 'CREATE DATABASE rbh_entries'
mariadb rbh_entries < /usr/share/robinhood/schema.sql
# Create the MariaDB service account and grant SELECT, INSERT, UPDATE, and DELETE
# on rbh_entries.*. Use its credentials in RBH_DATABASE_URL below.

# Create directories, start service.
systemd-tmpfiles --create robinhood.conf
editor /etc/sysconfig/rbh-daemon   # set RBH_DATABASE_URL, RBH_MDTS, ...
systemctl daemon-reload
systemctl enable --now rbh-daemon
```

## Build and serve the web console

Run `npm ci` and `npm run build` in `web/` with Node.js 22 before packaging.
Package the generated `web/dist/` files with the daemon. The Compose image
does this automatically. Open `/console/` on the daemon's existing HTTP
listener. Set `RBH_CONSOLE_DIR` to an absolute path to serve a different
installation directory, such as the local `web/dist/` for development.
The daemon only needs read access to the installed files. Node.js is a build
dependency.

The [container deployment](../README.md#deploy-a-container-that-mounts-lustre)
uses `docker/compose.console.yml` to serve the console over HTTP on a trusted
operator network. Set `RBH_CONSOLE_HTTP_LISTEN` to the host's private IP and port
and `RBH_CONSOLE_HTTP_HOST` to the hostname or IP operators enter in their
browsers. Set `RBH_CONSOLE_TRUSTED_CIDR` to the operator network. Every client
allowed by the gateway can write, and the daemon records those writes under
`trusted-gateway`. The gateway connects over a Unix socket in a volume shared
with the daemon. Keep `RBH_LISTEN_ADDR` on loopback for local reads. Use the
Unix socket from the Docker host to configure governance resources. Find its
host path with `docker inspect` by looking up the mount whose destination is
`/run/rbh-gateway` and appending `daemon.sock` to its source directory.

For a bearer-token deployment behind a separately managed HTTPS reverse proxy,
forward `/console/` and `/api/` on the same origin to the daemon. Preserve the
request URI and the `Authorization` and `If-Match` headers. Local browser access
can use `http://127.0.0.1:8080/console/`.

The daemon schedules Lustre resource collection through `scheduler-rs` at
startup and every 60 seconds by default. Set `RBH_CONSOLE_COLLECT_SECS` to a
positive number of seconds to change the interval. The console reads the
latest MariaDB snapshots. Refresh requests wait for the selected collection
to finish and then reload the saved snapshots.
Each collector uses its hostname to scope saved observations. Set a stable,
unique `RBH_CONSOLE_OBSERVER_ID` when daemon instances with the same hostname
share a MariaDB database or when a deployment has a changing hostname.

To exercise the browser flows against a running daemon with built assets,
run `npx playwright install chromium` and `npm run test:e2e` in `web/`.
`RBH_CONSOLE_TEST_URL` defaults to `http://127.0.0.1:8080`.
For live-data acceptance, set `RBH_CONSOLE_REQUIRE_LIVE_DATA=1` and use a
daemon with two eligible Lustre pools and a saved policy with an explicit pool pair.

### Daemon-managed Lustre mounts

The cluster list shows Lustre mounts discovered in the daemon's mount namespace.
Deployments can configure existing mounts through `RBH_FILESYSTEMS_JSON` for
policy execution and resource collection. To let the daemon create, unmount,
and restore its own mounts through the console, set
`RBH_LUSTRE_MANAGED_ROOT` to a canonical writable directory owned by the service
user, and grant the service host Lustre mount privileges. On Linux this requires
`CAP_SYS_ADMIN` in the mount namespace where the daemon runs. The packaged
systemd unit also needs the managed directory in `ReadWritePaths`. The daemon
runs the local Lustre client helpers and confines new mount paths to immediate
children of that directory. Enable resource management writes through the
trusted HTTP gateway or a configured writer token. Install the Lustre mount
helpers and their shared libraries from the same client release.

Unmounting an application-managed mount keeps the filesystem ID, catalog, policy
references, and run history. The daemon blocks unmounting while a scan or operation
is active or a referencing policy is enabled. A manually unmounted mount stays
unmounted across restarts until mounted again. To restore mounts after replacing
a Pod, reuse the same MariaDB database and `RBH_CONSOLE_OBSERVER_ID`.

## Enable pool migration under systemd

Add a drop-in with `systemctl edit rbh-daemon.service` before enabling an
enforced pool migration policy. List the executor mount for each Lustre
filesystem whose policies migrate files. The unit's mount namespace must
allow the daemon to create volatile files at these mount roots.

```ini
[Service]
ReadWritePaths=/mnt/lustre
```

Use the actual paths from `RBH_FILESYSTEMS_JSON` or `RBH_LUSTRE_MOUNT`.
For an instance unit, edit `rbh-daemon@INSTANCE.service`. The service
credentials must allow opening the source files for write and setting
the source owner and group on each temporary copy.

Directory-scoped policies also need live FID-to-path lookup on the
executor mount. Use the mount's `user_fid2path` setting if permitted by
the deployment. With `nouser_fid2path`, grant the executor service
`CAP_DAC_READ_SEARCH` if its privilege policy allows it. This capability
bypasses Linux read and directory-search permission checks. The daemon
probes live lookup when enabling a scoped policy and reports missing
access. Each migration checks the namespace again before exchanging
layouts.

For concurrent pool migration, set the policy's same-filesystem limit and bind
an execution profile with sufficient global slots. Each FID has its own durable
reservation and Lustre group lock. Concurrent copies share the policy's attempt,
operation, data and time budgets. A configured bandwidth limit is divided among
the permitted concurrent copies.

## Signals

| Signal   | Effect                                                        |
|----------|---------------------------------------------------------------|
| SIGTERM  | Graceful shutdown — flush changelog batcher, commit cursor.   |
| SIGINT   | Same as SIGTERM.                                              |
| SIGHUP   | Re-read `RBH_LOG` and reload the `tracing-subscriber` filter. |
| SIGUSR1  | Dump runtime stats via `tracing::info!`.                      |

`systemctl reload rbh-daemon` sends SIGHUP.
