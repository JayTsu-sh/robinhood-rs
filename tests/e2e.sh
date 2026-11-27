#!/usr/bin/env bash
# End-to-end smoke tests for robinhood-rs.
#
# What it does, in order:
#   1. Creates a dedicated test DB (RBH_E2E_DB, default rbh_e2e_<pid>).
#   2. Spawns the daemon against that DB on a non-default port (8088).
#   3. Seeds scoped catalog entries and one removed entry via mysql.
#   4. Exercises: /api/health, /api/entries/count, rbh find (multiple forms),
#      rbh report (fs-info / size-profile / top-users / dump), rbh undelete
#      (list / forget), rejection of the previous policy format, and
#      /api/metrics.
#   5. Tears the daemon down with SIGTERM and asserts graceful exit.
#
# Requirements on the host:
#   * Disposable MariaDB reachable at 127.0.0.1:3306 as root without a password.
#   * robinhood and rbh built in ${CARGO_TARGET_DIR:-target}/release.
#   * An ordinary empty directory serves as the Lustre mount placeholder.
#
# Exit codes:
#   0  all assertions passed.
#   !=0 at the first failed assertion (set -e).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT=${RBH_E2E_PORT:-8088}
DB=${RBH_E2E_DB:-rbh_e2e_${BASHPID}}
API="http://127.0.0.1:$PORT"
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN_DAEMON="$TARGET_DIR/release/robinhood"
BIN_CLI="$TARGET_DIR/release/rbh"
FILESYSTEM=lustre

# Keep API requests to the local daemon off any inherited proxy.
export NO_PROXY=127.0.0.1 no_proxy=127.0.0.1

PID=""
DB_CREATED=0
MOUNT=""

test_sql() {
    mysql --no-defaults --host=127.0.0.1 --port=3306 --user=root --skip-password "$@"
}

cleanup() {
    if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
        echo "[cleanup] SIGTERM -> $PID"
        kill -TERM "$PID" || true
        for _ in 1 2 3 4 5; do
            sleep 1
            kill -0 "$PID" 2>/dev/null || break
        done
        kill -0 "$PID" 2>/dev/null && kill -KILL "$PID" || true
    fi
    if [[ -n "$PID" ]]; then
        wait "$PID" 2>/dev/null || true
    fi
    if [[ "$DB_CREATED" == 1 && "${RBH_E2E_KEEP_DB:-0}" != "1" ]]; then
        test_sql -e "DROP DATABASE $DB;" || true
    fi
    if [[ -n "$MOUNT" ]]; then
        rmdir "$MOUNT" || true
    fi
}
trap cleanup EXIT

die() { echo "FAIL: $*" >&2; exit 1; }
assert_contains() {
    local hay="$1" needle="$2" tag="$3"
    [[ "$hay" == *"$needle"* ]] || die "$tag: expected to contain [$needle], got [$hay]"
}
assert_http() {
    local code="$1" want="$2" tag="$3"
    [[ "$code" == "$want" ]] || die "$tag: expected HTTP $want, got HTTP $code"
}
rbh() {
    RBH_LOG=warn "$BIN_CLI" --api-url "$API" --filesystem "$FILESYSTEM" "$@"
}

# 0. prerequisites
[[ -x "$BIN_DAEMON" ]] || die "missing $BIN_DAEMON — run: cargo build --release -p robinhood-rs"
[[ -x "$BIN_CLI" ]]    || die "missing $BIN_CLI    — run: cargo build --release -p rbh-cli"
command -v mysql >/dev/null 2>&1 || die "mysql CLI not found on PATH"
[[ "$DB" =~ ^[a-zA-Z_][a-zA-Z0-9_]{0,63}$ ]] || die "invalid test database name: $DB"
[[ "$PORT" =~ ^[1-9][0-9]{0,4}$ ]] && (( PORT <= 65535 )) || die "invalid test port: $PORT"
if ( : </dev/tcp/127.0.0.1/"$PORT" ) 2>/dev/null; then
    die "test port $PORT is already in use"
fi
LOG="$(mktemp "${TMPDIR:-/tmp}/rbh-e2e.XXXXXX.log")"
MOUNT="$(mktemp -d "${TMPDIR:-/tmp}/rbh-e2e-mount.XXXXXX")"

# 1. bring up a fresh DB
echo "[setup] creating DB $DB (daemon log: $LOG)"
test_sql -e "CREATE DATABASE $DB;" \
    || die "cannot create DB $DB (use a fresh name on a disposable MariaDB)"
DB_CREATED=1

# 2. launch daemon (migrations run on boot)
echo "[setup] launching daemon on :$PORT"
RBH_DATABASE_URL="mysql://root@127.0.0.1/$DB" \
RBH_FILESYSTEMS_JSON="" \
RBH_FILESYSTEM_ID="$FILESYSTEM" \
RBH_LUSTRE_MOUNT="$MOUNT" \
RBH_CHANGELOG_USER="" \
RBH_LOG="info" \
RBH_LISTEN_ADDR="127.0.0.1:$PORT" \
    env -u RBH_REV8_WRITERS_FILE -u RBH_REV8_HSM_PRINCIPAL_FILE \
    "$BIN_DAEMON" >"$LOG" 2>&1 &
PID=$!

for i in $(seq 1 30); do
    kill -0 "$PID" 2>/dev/null || die "daemon exited before becoming healthy (see $LOG)"
    if grep -q 'HTTP server listening' "$LOG" &&
        curl -fsS --max-time 1 "$API/api/health" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
    [[ $i == 30 ]] && die "daemon never became healthy (see $LOG)"
done
echo "[setup] daemon healthy (pid $PID)"

# 3. Seed scoped rows with matching typed metadata for queries and JSON reads.
echo "[seed] inserting scoped entries and one removed entry for $FILESYSTEM"
test_sql "$DB" <<SQL
CREATE TEMPORARY TABLE fixture (
    oid CHAR(2), name VARCHAR(30), kind TINYINT, size BIGINT UNSIGNED,
    uid INT UNSIGNED, gid INT UNSIGNED, mode INT UNSIGNED, nlink INT UNSIGNED
);
INSERT INTO fixture VALUES
    ('11', 'small.txt',   0,      1024, 1000, 100, 33188, 1),
    ('12', 'medium.log',  0,   1048576, 1000, 100, 33188, 1),
    ('13', 'large.bin',   0, 209715200, 1000, 100, 33188, 1),
    ('14', 'mail.out',    0,    512000, 1000, 100, 33188, 1),
    ('15', 'misc.dat',    0,    256000, 1000, 100, 33188, 1),
    ('21', 'etc',         1,      4096,    0,   0, 16877, 3),
    ('22', 'root.cfg',    0,     65536,    0,   0, 33188, 1),
    ('23', 'kernel.img',  0,  12582912,    0,   0, 33188, 1),
    ('31', 'deleted.tmp', 0,        42, 1000, 100, 33188, 1);

START TRANSACTION;
INSERT INTO scoped_entries
    (filesystem_id, object_kind, object_id, fid, name, kind, size, uid, gid,
     mode, nlink, atime, mtime, ctime, last_seen, depth, entry_data)
SELECT '$FILESYSTEM', 0, fid, fid, name, kind, size, uid, gid,
       mode, nlink, 1700000000, 1700000000, 1700000000, 1700000001, 1,
       JSON_OBJECT(
           'key', JSON_OBJECT('filesystem', '$FILESYSTEM', 'object',
                              JSON_OBJECT('lustre', CONCAT('0x200000101:0x', oid, ':0x0'))),
           'name', name, 'kind', IF(kind = 0, 'File', 'Directory'), 'size', size,
           'blocks', 0, 'uid', uid, 'gid', gid, 'projid', 0, 'mode', mode, 'nlink', nlink,
           'atime', 1700000000, 'mtime', 1700000000, 'ctime', 1700000000,
           'last_seen', 1700000001, 'depth', 1, 'sm_status', JSON_OBJECT())
FROM (SELECT fixture.*, UNHEX(CONCAT('0000000200000101000000', oid, '00000000')) AS fid
      FROM fixture) AS seed;

SET @removed_fid = X'00000002000001010000003100000000';
INSERT INTO scoped_removed_entries (filesystem_id, object_kind, object_id, entry_data, rm_time)
SELECT filesystem_id, object_kind, object_id, entry_data, 1700000000
FROM scoped_entries
WHERE filesystem_id = '$FILESYSTEM' AND object_kind = 0 AND object_id = @removed_fid;
DELETE FROM scoped_entries
WHERE filesystem_id = '$FILESYSTEM' AND object_kind = 0 AND object_id = @removed_fid;
COMMIT;
SQL

# 4a. health + count
body="$(curl -fsS "$API/api/health")"
assert_contains "$body" '"ok"' "health"

count_resp="$(curl -fsS "$API/api/entries/count?filesystem=$FILESYSTEM")"
assert_contains "$count_resp" '"count":8' "entry count 8"

# 4b. find
out="$(rbh find --user 1000 --limit 10 --json)"
assert_contains "$out" '"small.txt"' "find --user 1000 returned small.txt"
assert_contains "$out" '"large.bin"' "find --user 1000 returned large.bin"

out="$(rbh find --type d)"
assert_contains "$out" "etc" "find --type d returned dir"

out="$(rbh find --size +1M)"
assert_contains "$out" "large.bin" "find --size +1M returned large.bin"
[[ "$out" == *"small.txt"* ]] && die "--size +1M should exclude small.txt"

# 4c. report fs-info
out="$(rbh report fs-info)"
assert_contains "$out" "file" "fs-info has 'file' row"
assert_contains "$out" "dir"  "fs-info has 'dir' row"

# 4d. report size-profile (large.bin=200M -> '100M-1G'; kernel.img=12M -> '1M-100M')
out="$(rbh report size-profile)"
assert_contains "$out" "100M-1G"  "size-profile has 100M-1G bucket"
assert_contains "$out" "1M-100M"  "size-profile has 1M-100M bucket"

# 4e. report top-users
out="$(rbh report top-users --n 5)"
assert_contains "$out" "1000" "top-users has uid 1000"

# 4f. report dump --user
out="$(rbh report dump --user 0 --limit 10)"
assert_contains "$out" "kernel.img" "dump --user 0 returned kernel.img"

# 4g. undelete list / forget
out="$(rbh undelete list --n 10)"
assert_contains "$out" "deleted.tmp" "undelete list returned deleted.tmp"

# forget existing -> 204
http="$(curl -sS -o /dev/null -w "%{http_code}" -X DELETE \
        "$API/api/removed/0x200000101:0x31:0x0?filesystem=$FILESYSTEM")"
assert_http "$http" "204" "DELETE /api/removed existing"

# forget unknown -> 404
http="$(curl -sS -o /dev/null -w "%{http_code}" -X DELETE \
        "$API/api/removed/0x0:0x0:0x0?filesystem=$FILESYSTEM")"
assert_http "$http" "404" "DELETE /api/removed unknown"

# after deletion, list is empty
out="$(curl -fsS "$API/api/removed?filesystem=$FILESYSTEM")"
assert_contains "$out" '"rows":[]' "removed list empty after forget"

# 4h. policy format and metrics
legacy_error="$(curl -sS -X POST "$API/api/policies" -H 'Content-Type: application/json' \
    -d '{"name":"e2e_threshold","kind":"alert","filesystem":"lustre","trigger":"count > 3"}' \
    -w '\n%{http_code}')"
assert_http "${legacy_error##*$'\n'}" "400" "legacy policy rejected"
assert_contains "$legacy_error" "hsm.storage.io/v1alpha3" "policy format error identifies Rev.8"
metrics="$(curl -fsS "$API/api/metrics")"
assert_contains "$metrics" 'rbh_catalog_entries{backend="lustre",filesystem="lustre"} 8' "/metrics contains scoped catalog count"

# 5. graceful shutdown
echo "[teardown] SIGTERM"
kill -TERM "$PID"
for _ in $(seq 1 10); do
    sleep 0.5
    kill -0 "$PID" 2>/dev/null || break
done
if kill -0 "$PID" 2>/dev/null; then
    die "daemon did not exit within 5s of SIGTERM"
fi
wait "$PID" || die "daemon exited unsuccessfully (see $LOG)"
PID=""

# check log for the graceful-shutdown banner
grep -q "robinhood-rs daemon stopped" "$LOG" \
    || die "daemon log missing 'daemon stopped' line (see $LOG)"

echo "[done] all assertions passed"
exit 0
