# Local dev stack

A four-service compose file that lets you exercise robinhood-rs without a
real Lustre cluster: MariaDB, the rbh-daemon, Prometheus, and Grafana
preloaded with the dashboard from `packaging/grafana`.

## Quick start

```bash
cd dev/compose
docker compose up -d --build
```

First build takes ~5 min (cold Rust compile). After that:

| Service | URL |
|---------|-----|
| rbh-daemon API | http://localhost:8080/api/health |
| Prometheus     | http://localhost:9090 |
| Grafana        | http://localhost:3000 (admin / admin) |

The Grafana dashboard is provisioned automatically from
`grafana/dashboards/robinhood-rs.json` (a copy of the packaged JSON).

## What's intentionally absent

- **Lustre.** The daemon's `liblustreapi` symbols are fulfilled by a stub
  that returns `-1` on every call. FS-scan and changelog would fail —
  the compose stack keeps `RBH_MDTS` / `RBH_CHANGELOG_USER` unset so the
  listener never starts. You can still hit the REST API, create policies,
  exercise the threshold checker, and watch metrics accrue.
- **OSS hosts.** No simulated OSTs; `OnOst` predicates return empty.

For a real end-to-end run, run the daemon binary on a node that has
Lustre mounted and point this compose's MariaDB at it instead, or
replace the daemon service with the host binary.

## Seeding fake data

With the stack up:

```bash
mysql -h 127.0.0.1 -P 3306 -u root -prbhdev rbh_entries <<'SQL'
INSERT INTO entries
  (fid, name, kind, size, uid, gid, projid, mode, nlink,
   atime, mtime, ctime, last_seen, sm_status)
VALUES
  (X'00000002000000420000002A00000000', 'demo.txt',
   0, 1024, 1000, 1000, 0, 0o100644, 1,
   UNIX_TIMESTAMP(), UNIX_TIMESTAMP(), UNIX_TIMESTAMP(), UNIX_TIMESTAMP(),
   '{}');
SQL
curl -s http://localhost:8080/api/metrics | grep rbh_catalog_entries
```

## Tearing down

```bash
docker compose down         # stop, keep volumes
docker compose down -v      # stop + wipe MariaDB / Grafana state
```
