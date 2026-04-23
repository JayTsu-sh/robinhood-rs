#!/usr/bin/env bash
# End-to-end smoke tests for robinhood-rs.
#
# What it does, in order:
#   1. Creates a dedicated test DB (RBH_E2E_DB, default rbh_e2e_test).
#   2. Spawns the daemon against that DB on a non-default port (8088).
#   3. Seeds a handful of entries + one removed_entry directly via mysql.
#   4. Exercises: /api/health, /api/entries/count, rbh find (multiple forms),
#      rbh report (fs-info / top-size / size-profile / dump), rbh undelete
#      (list / forget), a threshold policy that fires immediately, and
#      /api/metrics.
#   5. Tears the daemon down with SIGTERM and asserts graceful exit.
#
# Requirements on the host:
#   * MariaDB / MySQL reachable as root (password optional via MYSQL_PWD env).
#   * target/release/robinhood and target/release/rbh built.
#   * A Lustre mount is NOT required — RBH_CHANGELOG_USER stays unset.
#
# Exit codes:
#   0  all assertions passed.
#   !=0 at the first failed assertion (set -e).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT=${RBH_E2E_PORT:-8088}
DB=${RBH_E2E_DB:-rbh_e2e_test}
API="http://127.0.0.1:$PORT"
LOG="/tmp/rbh-e2e.log"
BIN_DAEMON="$ROOT/target/release/robinhood"
BIN_CLI="$ROOT/target/release/rbh"

PID=""

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
    if [[ "${RBH_E2E_KEEP_DB:-0}" != "1" ]]; then
        mysql -u root -e "DROP DATABASE IF EXISTS $DB;" 2>/dev/null || true
    fi
}
trap cleanup EXIT

die() { echo "FAIL: $*" >&2; exit 1; }
assert_eq() {
    local got="$1" want="$2" tag="$3"
    [[ "$got" == "$want" ]] || die "$tag: expected [$want], got [$got]"
}
assert_contains() {
    local hay="$1" needle="$2" tag="$3"
    [[ "$hay" == *"$needle"* ]] || die "$tag: expected to contain [$needle], got [$hay]"
}
assert_http() {
    local code="$1" want="$2" tag="$3"
    [[ "$code" == "$want" ]] || die "$tag: expected HTTP $want, got HTTP $code"
}

# 0. prerequisites
[[ -x "$BIN_DAEMON" ]] || die "missing $BIN_DAEMON — run: cargo build --release -p robinhood-rs"
[[ -x "$BIN_CLI" ]]    || die "missing $BIN_CLI    — run: cargo build --release -p rbh-cli"
command -v mysql >/dev/null 2>&1 || die "mysql CLI not found on PATH"

# 1. bring up a fresh DB
echo "[setup] fresh DB $DB"
mysql -u root -e "DROP DATABASE IF EXISTS $DB; CREATE DATABASE $DB;" \
    || die "cannot create DB $DB (is mysql reachable as root?)"

# 2. launch daemon (migrations run on boot)
echo "[setup] launching daemon on :$PORT"
RBH_DATABASE_URL="mysql://root@127.0.0.1/$DB" \
RBH_LUSTRE_MOUNT="/tmp/rbh-e2e-fake" \
RBH_LOG="info" \
RBH_LISTEN_ADDR="127.0.0.1:$PORT" \
RBH_THRESHOLD_TICK_SECS="2" \
    nohup "$BIN_DAEMON" >"$LOG" 2>&1 &
PID=$!
mkdir -p /tmp/rbh-e2e-fake

for i in $(seq 1 30); do
    if curl -fsS --max-time 1 "$API/api/health" >/dev/null 2>&1; then
        break
    fi
    sleep 0.5
    [[ $i == 30 ]] && die "daemon never became healthy (see $LOG)"
done
echo "[setup] daemon healthy (pid $PID)"

# 3. seed: 5 files owned by uid=1000, 3 by uid=0, plus one removed entry
echo "[seed] inserting entries + removed_entries"
mysql -u root -e "
USE $DB;
INSERT INTO entries (fid, name, kind, size, uid, gid, projid, mode, nlink,
                     atime, mtime, ctime, last_seen, sm_status)
VALUES
 (UNHEX('00000002000001010000001100000000'), 'small.txt',   0,    1024, 1000, 100, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000001200000000'), 'medium.log',  0, 1048576, 1000, 100, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000001300000000'), 'large.bin',   0, 209715200,1000, 100, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000001400000000'), 'mail.out',    0,  512000,  1000, 100, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000001500000000'), 'misc.dat',    0,  256000,  1000, 100, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000002100000000'), 'etc',         1,    4096,     0,   0, 0, 16877, 3, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000002200000000'), 'root.cfg',    0,   65536,     0,   0, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}'),
 (UNHEX('00000002000001010000002300000000'), 'kernel.img',  0,12582912,    0,   0, 0, 33188, 1, 1700000000, 1700000000, 1700000000, 1700000001, '{}');

INSERT INTO removed_entries (fid, name, kind, size, uid, gid, sm_status, rm_time)
VALUES
 (UNHEX('00000002000001010000003100000000'), 'deleted.tmp', 0, 42, 1000, 100, '{}', 1700000000);
"

# 4. assertions start here
fail_count=0
run() {
    local tag="$1"; shift
    echo "--- $tag"
    if "$@"; then
        :
    else
        echo "    -> FAIL" && fail_count=$((fail_count+1))
    fi
}

# 4a. health + count
body="$(curl -sS "$API/api/health")"
assert_contains "$body" '"ok"' "health"

count_resp="$(curl -sS "$API/api/entries/count")"
assert_contains "$count_resp" '"count":8' "entry count 8"

# 4b. find
out="$($BIN_CLI --api-url "$API" find --user 1000 --limit 10 --json 2>&1)"
assert_contains "$out" '"small.txt"' "find --user 1000 returned small.txt"
assert_contains "$out" '"large.bin"' "find --user 1000 returned large.bin"

out="$($BIN_CLI --api-url "$API" find --type d 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "etc" "find --type d returned dir"

out="$($BIN_CLI --api-url "$API" find --size +1M 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "large.bin" "find --size +1M returned large.bin"
[[ "$out" == *"small.txt"* ]] && die "--size +1M should exclude small.txt"

# 4c. report fs-info
out="$($BIN_CLI --api-url "$API" report fs-info 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "file" "fs-info has 'file' row"
assert_contains "$out" "dir"  "fs-info has 'dir' row"

# 4d. report size-profile (large.bin=200M -> '100M-1G'; kernel.img=12M -> '1M-100M')
out="$($BIN_CLI --api-url "$API" report size-profile 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "100M-1G"  "size-profile has 100M-1G bucket"
assert_contains "$out" "1M-100M"  "size-profile has 1M-100M bucket"

# 4e. report top-users
out="$($BIN_CLI --api-url "$API" report top-users --n 5 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "1000" "top-users has uid 1000"

# 4f. report dump --user
out="$($BIN_CLI --api-url "$API" report dump --user 0 --limit 10 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "kernel.img" "dump --user 0 returned kernel.img"

# 4g. undelete list / forget
out="$($BIN_CLI --api-url "$API" undelete list --n 10 2>/dev/null \
     | grep -v observability | grep -v crates/)"
assert_contains "$out" "deleted.tmp" "undelete list returned deleted.tmp"

# forget existing -> 204
http="$(curl -sS -o /dev/null -w "%{http_code}" -X DELETE \
        "$API/api/removed/%5B0x200000101:0x31:0x0%5D")"
assert_http "$http" "204" "DELETE /api/removed existing"

# forget unknown -> 404
http="$(curl -sS -o /dev/null -w "%{http_code}" -X DELETE \
        "$API/api/removed/%5B0x0:0x0:0x0%5D")"
assert_http "$http" "404" "DELETE /api/removed unknown"

# after deletion, list is empty
out="$(curl -sS "$API/api/removed")"
assert_eq "$out" "[]" "removed list empty after forget"

# 4h. threshold fire + metrics
echo "--- threshold policy"
id="$(curl -sS -X POST "$API/api/policies" -H 'content-type: application/json' \
  -d '{"name":"e2e_threshold","kind":"alert","scope":{"op":"true"},"rules":[],
       "default_action":{"max_count":1},
       "triggers":[{"type":"threshold_count","check_interval_secs":2,
                    "high_count":3,"low_count":0,"post_trigger_wait_secs":999,
                    "target":{"kind":"fs"}}]}' \
   | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')"
[[ -n "$id" ]] || die "threshold policy creation returned no id"

# wait up to 10s for the fire counter to bump. `grep -F` + `|| true`
# prevents `set -e -o pipefail` from killing the script on a miss.
hit=""
needle="rbh_threshold_fires_total{policy_id=\"$id\"}"
for _ in $(seq 1 20); do
    sleep 0.5
    val="$(curl -sS "$API/api/metrics" | grep -F "$needle" | awk '{print $NF}' | head -1 || true)"
    if [[ -n "${val:-}" && "${val:-}" != "0" ]]; then
        hit="$val"
        break
    fi
done
[[ -n "$hit" ]] || die "threshold never fired within 10s"
echo "    -> threshold fired (count=$hit)"

# metrics must include at least these families
metrics="$(curl -sS "$API/api/metrics")"
for fam in rbh_catalog_entries rbh_threshold_fires_total rbh_policy_runs_total; do
    assert_contains "$metrics" "$fam" "/metrics contains $fam"
done

# cleanup policy
curl -sS -X DELETE "$API/api/policies/$id" >/dev/null

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
PID=""

# check log for the graceful-shutdown banner
grep -q "robinhood-rs daemon stopped" "$LOG" \
    || die "daemon log missing 'daemon stopped' line (see $LOG)"

echo "[done] all assertions passed"
exit 0
