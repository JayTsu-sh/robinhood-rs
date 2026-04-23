# Packaging assets for robinhood-rs

```
systemd/
├── rbh-daemon.service      → /usr/lib/systemd/system/rbh-daemon.service
└── rbh-daemon@.service     → /usr/lib/systemd/system/rbh-daemon@.service
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

## Signals

| Signal   | Effect                                                        |
|----------|---------------------------------------------------------------|
| SIGTERM  | Graceful shutdown — flush changelog batcher, commit cursor.   |
| SIGINT   | Same as SIGTERM.                                              |
| SIGHUP   | Re-read `RBH_LOG` and reload the `tracing-subscriber` filter. |
| SIGUSR1  | Dump runtime stats via `tracing::info!`.                      |

`systemctl reload rbh-daemon` sends SIGHUP.
