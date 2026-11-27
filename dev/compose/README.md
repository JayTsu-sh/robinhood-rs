# Local dev stack

A four-service compose file that lets you exercise robinhood-rs without a
real Lustre cluster: MariaDB, the rbh-daemon, Prometheus, and Grafana
preloaded with the dashboard from `packaging/grafana`.

## Quick start

The daemon image targets Linux x86-64. Build it with Docker Compose and
BuildKit using a Dockerfile frontend that supports `ADD --checksum` (v1.6 or
later). The build downloads a Rust base image, the pinned Cargo dependencies,
and the [Lustre 2.16.1 headers and user-space libraries](https://downloads.whamcloud.com/public/lustre/lustre-2.16.1/ubuntu2404/client/).

```bash
cd dev/compose
docker compose up -d --build
```

After the first build and database migration:

| Service | URL |
|---------|-----|
| rbh-daemon API | http://localhost:8080/api/health |
| Prometheus     | http://localhost:9090 |
| Grafana        | http://localhost:3000 (admin / admin) |

The Grafana dashboard is provisioned automatically from
`grafana/dashboards/robinhood-rs.json` (a copy of the packaged JSON).

Run a catalog and action-contract smoke check.

```bash
docker compose exec rbh-daemon rbh --filesystem lustre status
curl -fsS http://localhost:8080/api/action-contracts/lustre.pool_migrate
```

The image links to the Lustre user-space libraries. The stack has an empty
placeholder directory for the `lustre` scope and no Lustre mount, MDT reader
or OSS. Use it to query a seeded catalog and preview native HSM policy
candidates. Run changelog, heat sampling, HSM execution and pool migration
tests on a node with a real Lustre mount.

An empty catalog triggers an initial scan. Expect scan retries until the
daemon restarts with a seeded catalog.

## Seeding fake data

Add one synthetic 2 GiB catalog file to the isolated development database.
It matches the archive preview in the Rev.8 REST example.

```bash
docker compose exec -T mariadb mariadb -uroot -prbhdev rbh_entries <<'SQL'
SET @fid = X'00000002000000420000002A00000000';
SET @size = 2147483648;
SET @now = UNIX_TIMESTAMP();
INSERT INTO scoped_entries
  (filesystem_id, object_kind, object_id, fid, name, kind, size, mode,
   atime, mtime, ctime, last_seen, depth, entry_data)
VALUES
  ('lustre', 0, @fid, @fid, 'demo.txt', 0, @size, 33188,
   @now, @now, @now, @now, 1,
   JSON_OBJECT(
     'key', JSON_OBJECT('filesystem', 'lustre',
                        'object', JSON_OBJECT('lustre', '0x200000042:0x2a:0x0')),
     'name', 'demo.txt', 'kind', 'File', 'size', CAST(@size AS UNSIGNED), 'blocks', 0,
     'uid', 0, 'gid', 0, 'projid', 0, 'mode', 33188, 'nlink', 1,
     'atime', CAST(@now AS SIGNED), 'mtime', CAST(@now AS SIGNED),
     'ctime', CAST(@now AS SIGNED), 'last_seen', CAST(@now AS SIGNED),
     'sm_status', JSON_OBJECT(), 'depth', 1))
ON DUPLICATE KEY UPDATE object_id = VALUES(object_id);
SQL
curl -fsS 'http://localhost:8080/api/entries/0x200000042:0x2a:0x0?filesystem=lustre'
```

## Previewing a Rev.8 policy

From `dev/compose`, save the archive policy example as
`archive-preview.json` and add a writer to the local daemon.
These commands use `openssl` and `jq`. [Rev.8 REST example](../../README.md#rev8-native-lustre-hsm)

```bash
dev_auth=$(mktemp -d)
dev_token=$(openssl rand -hex 32)
dev_digest=$(printf '%s' "$dev_token" | sha256sum | cut -d ' ' -f 1)
printf '{"compose-dev":"%s"}\n' "$dev_digest" > "$dev_auth/writers.json"
cat > "$dev_auth/auth.yml" <<EOF
services:
  rbh-daemon:
    environment:
      RBH_REV8_WRITERS_FILE: /run/rbh-writers.json
    volumes:
      - "$dev_auth/writers.json:/run/rbh-writers.json:ro"
EOF
docker compose -f docker-compose.yml -f "$dev_auth/auth.yml" up -d rbh-daemon
```

Once the daemon is healthy, create the policy and preview its candidates.
On a freshly seeded catalog, expect one candidate and 2147483648 estimated bytes.

```bash
policy_id=$(curl -fsS http://localhost:8080/api/policies \
  -H "Authorization: Bearer $dev_token" -H 'Content-Type: application/json' \
  --data-binary @archive-preview.json | jq -er '.id')
curl -fsS -X POST "http://localhost:8080/api/policies/$policy_id/preview"
```

## Tearing down

```bash
docker compose down         # stop, keep volumes
docker compose down -v      # stop + wipe MariaDB / Grafana state
```
