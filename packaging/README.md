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
install -m 0644 packaging/systemd/rbh-daemon.service  /usr/lib/systemd/system/
install -m 0644 packaging/systemd/rbh-daemon@.service /usr/lib/systemd/system/
install -m 0640 packaging/sysconfig/rbh-daemon        /etc/sysconfig/rbh-daemon
install -m 0644 packaging/tmpfiles.d/robinhood.conf   /usr/lib/tmpfiles.d/
install -m 0644 packaging/logrotate/rbh-daemon        /etc/logrotate.d/

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

## Signals

| Signal   | Effect                                                        |
|----------|---------------------------------------------------------------|
| SIGTERM  | Graceful shutdown — flush changelog batcher, commit cursor.   |
| SIGINT   | Same as SIGTERM.                                              |
| SIGHUP   | Re-read `RBH_LOG` and reload the `tracing-subscriber` filter. |
| SIGUSR1  | Dump runtime stats via `tracing::info!`.                      |

`systemctl reload rbh-daemon` sends SIGHUP.
