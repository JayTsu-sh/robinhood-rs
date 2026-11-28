#!/usr/bin/env bash
# Full-stack integration test for robinhood-rs against a real Lustre mount.
#
# Scope — what this covers that `tests/e2e.sh` does not:
#   1. Changelog ingest: create / rename / unlink events round-trip
#      Lustre → listener → DB.
#   2. An explicit scan populates the catalog. Compatibility sweep trims stale rows.
#   3. Catalog dump/restore round-trip preserves entries.
#   4. Unsupported policy format rejection.
#
# Not covered (would need lhsmtool_cmd running):
#   * HSM archive / release state transitions
#   * HSM poller reconciliation
#
# Prerequisites on the test host:
#   * A writable Lustre mount at $LUSTRE_MOUNT (default /lustre)
#   * A pre-registered changelog user (see memory/lustre_operator_setup.md);
#     pass its id via RBH_CHANGELOG_USER (e.g. cl7)
#   * MariaDB reachable as root
#   * target/release/{robinhood,rbh} built (make with `cargo build --release`)
#
# Env knobs:
#   LUSTRE_MOUNT           — /lustre by default
#   RBH_CHANGELOG_USER     — required; e.g. cl7
#   RBH_MDTS               — optional comma list; defaults to testfs-MDT0000
#   RBH_E2E_PORT           — HTTP port (default 8089)
#   RBH_E2E_DB_PORT        — local MariaDB TCP port (default 3306)
#   RBH_E2E_DB             — DB name (default rbh_e2e_lustre_<pid>)
#   RBH_E2E_CHANGELOG_SETTLE — seconds to wait for an event to reach the DB
#                             after an fs mutation (default 6)
#   RBH_E2E_KEEP_DB=1      — don't drop the DB on exit (debugging)
#
# Usage:
#   RBH_CHANGELOG_USER=cl7 ./tests/e2e_lustre.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT=${RBH_E2E_PORT:-8089}
DB_PORT=${RBH_E2E_DB_PORT:-3306}
DB=${RBH_E2E_DB:-rbh_e2e_lustre_${BASHPID}}
FILESYSTEM=lustre
API="http://127.0.0.1:$PORT"
LOG="/tmp/rbh-e2e-lustre.log"
BIN_DAEMON="$ROOT/target/release/robinhood"
BIN_CLI="$ROOT/target/release/rbh"

MOUNT=${LUSTRE_MOUNT:-/lustre}
MDTS=${RBH_MDTS:-testfs-MDT0000}
SETTLE=${RBH_E2E_CHANGELOG_SETTLE:-6}

PID=""
TEST_DIR=""
DB_CREATED=0

test_sql() {
    mysql --no-defaults --host=127.0.0.1 --port="$DB_PORT" --user=root --skip-password "$@"
}

cleanup() {
    if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
        echo "[cleanup] SIGTERM -> $PID"
        kill -TERM "$PID" || true
        for _ in 1 2 3 4 5 6 7 8; do
            sleep 1
            kill -0 "$PID" 2>/dev/null || break
        done
        kill -0 "$PID" 2>/dev/null && kill -KILL "$PID" || true
    fi
    if [[ -n "$TEST_DIR" && -d "$TEST_DIR" ]]; then
        rm -rf "$TEST_DIR" || true
    fi
    if [[ "$DB_CREATED" == 1 && "${RBH_E2E_KEEP_DB:-0}" != "1" ]]; then
        test_sql -e "DROP DATABASE IF EXISTS $DB;" 2>/dev/null || true
    fi
}
trap cleanup EXIT

die() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[$(date +%H:%M:%S)] $*"; }
assert_eq() {
    local got="$1" want="$2" tag="$3"
    [[ "$got" == "$want" ]] || die "$tag: expected [$want], got [$got]"
}
assert_ge() {
    local got="$1" want="$2" tag="$3"
    [[ "$got" -ge "$want" ]] || die "$tag: expected >= $want, got $got"
}
assert_contains() {
    local hay="$1" needle="$2" tag="$3"
    [[ "$hay" == *"$needle"* ]] || die "$tag: expected to contain [$needle], got [$hay]"
}

# Wait up to $SETTLE seconds for the DB row to satisfy a predicate.
# Usage: wait_for_entry <sql predicate>
wait_for_entry() {
    local sql="SELECT COUNT(*) FROM scoped_entries WHERE filesystem_id = '$FILESYSTEM' AND ($1)"
    local deadline=$((SECONDS + SETTLE))
    while [[ $SECONDS -lt $deadline ]]; do
        local n
        n=$(test_sql -N -B "$DB" -e "$sql" 2>/dev/null || echo 0)
        if [[ "$n" -gt 0 ]]; then
            return 0
        fi
        sleep 1
    done
    die "wait_for_entry: timeout waiting for [$1]"
}

wait_for_absence() {
    local sql="SELECT COUNT(*) FROM scoped_entries WHERE filesystem_id = '$FILESYSTEM' AND ($1)"
    local deadline=$((SECONDS + SETTLE))
    while [[ $SECONDS -lt $deadline ]]; do
        local n
        n=$(test_sql -N -B "$DB" -e "$sql" 2>/dev/null || echo 1)
        if [[ "$n" == 0 ]]; then
            return 0
        fi
        sleep 1
    done
    die "wait_for_absence: row still present matching [$1]"
}

# ---- 0. prerequisites ----
[[ -x "$BIN_DAEMON" ]] || die "missing $BIN_DAEMON — run: cargo build --release -p robinhood-rs"
[[ -x "$BIN_CLI" ]]    || die "missing $BIN_CLI    — run: cargo build --release -p rbh-cli"
command -v mysql >/dev/null 2>&1 || die "mysql CLI not found on PATH"
[[ -d "$MOUNT" ]] || die "Lustre mount $MOUNT does not exist"
[[ -n "${RBH_CHANGELOG_USER:-}" ]] || die "RBH_CHANGELOG_USER not set (e.g. cl7)"
[[ "$DB" =~ ^[a-zA-Z_][a-zA-Z0-9_]{0,63}$ ]] || die "invalid test database name: $DB"
[[ "$DB_PORT" =~ ^[1-9][0-9]{0,4}$ ]] && (( DB_PORT <= 65535 )) || die "invalid test database port: $DB_PORT"

TEST_DIR=$(mktemp -d "$MOUNT/rbh-e2e-XXXXXX") || die "cannot mktemp under $MOUNT"
note "workspace: $TEST_DIR"

# ---- 1. fresh DB ----
note "creating fresh DB $DB"
test_sql -e "CREATE DATABASE $DB;" || die "cannot create disposable DB $DB"
DB_CREATED=1
test_sql "$DB" < "$ROOT/crates/rbh-entry-store/schema.sql" || die "cannot initialize DB $DB"

# ---- 2. launch daemon with changelog enabled ----
note "launching daemon on :$PORT (mount=$MOUNT, mdts=$MDTS, user=$RBH_CHANGELOG_USER)"
RBH_DATABASE_URL="mysql://root@127.0.0.1:$DB_PORT/$DB" \
RBH_FILESYSTEMS_JSON="" \
RBH_FILESYSTEM_ID="$FILESYSTEM" \
RBH_LUSTRE_MOUNT="$MOUNT" \
RBH_MDTS="$MDTS" \
RBH_CHANGELOG_USER="$RBH_CHANGELOG_USER" \
RBH_LOG="info" \
RBH_LISTEN_ADDR="127.0.0.1:$PORT" \
    "$BIN_DAEMON" >"$LOG" 2>&1 &
PID=$!

for _ in $(seq 1 30); do
    if curl -sf "$API/api/health" >/dev/null; then break; fi
    sleep 1
done
curl -sf "$API/api/health" >/dev/null || {
    echo "------ daemon log ------"
    cat "$LOG"
    die "daemon failed to answer /api/health within 30s"
}
note "daemon ready"

# ---- 3. changelog: create, modify, rename, unlink ----
note "scenario 1: create"
SRC="$TEST_DIR/hello.txt"
echo "hello world" > "$SRC"
# grab the FID literal for the file we just made so we can chase it by name.
FID=$(lfs path2fid "$SRC" | tr -d '[]')
NAME=$(basename "$SRC")
wait_for_entry "name = '$NAME'"
note "  -> entry observed for $NAME (fid=$FID)"

note "scenario 2: rewrite changes size"
dd if=/dev/zero of="$SRC" bs=1k count=16 2>/dev/null
sleep "$SETTLE"
SIZE=$(test_sql -N -B "$DB" -e "SELECT size FROM scoped_entries WHERE filesystem_id='$FILESYSTEM' AND name='$NAME'")
assert_ge "$SIZE" "16384" "rewrite updated size"
note "  -> size updated to $SIZE bytes"

note "scenario 3: rename"
DST="$TEST_DIR/hello_renamed.txt"
mv "$SRC" "$DST"
NEWNAME=$(basename "$DST")
wait_for_entry "name = '$NEWNAME'"
wait_for_absence "name = '$NAME'"
OBJECT_HEX=$(test_sql -N -B "$DB" -e "SELECT HEX(object_id) FROM scoped_entries WHERE filesystem_id='$FILESYSTEM' AND name='$NEWNAME'")
[[ "$OBJECT_HEX" =~ ^[0-9A-F]{32}$ ]] || die "missing object ID for $NEWNAME"
note "  -> rename observed ($NAME -> $NEWNAME)"

note "scenario 4: unlink"
rm "$DST"
wait_for_absence "name = '$NEWNAME'"
RM_COUNT=$(test_sql -N -B "$DB" -e "SELECT COUNT(*) FROM scoped_removed_entries WHERE filesystem_id='$FILESYSTEM' AND object_id=UNHEX('$OBJECT_HEX')")
assert_ge "$RM_COUNT" "1" "unlinked entry persisted in scoped_removed_entries"
note "  -> scoped_removed_entries has the FID"

# ---- 4. fs-scan + orphan sweep ----
note "scenario 5: full scan populates catalog"
# Create a few more entries out-of-band (they arrive via changelog too, but
# scan should be a no-op on items already present).
for i in 1 2 3 4 5; do echo "x" > "$TEST_DIR/scan-$i.txt"; done
SCAN_START=$(date +%s)
SCAN_ID=$(curl -sf -X POST "$API/api/scans" -H 'content-type: application/json' \
            -d "$(python3 -c 'import json,sys; print(json.dumps({"root":sys.argv[1],"filesystem":sys.argv[2]}))' "$TEST_DIR" "$FILESYSTEM")" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> scan id $SCAN_ID"
# Poll for completion
for _ in $(seq 1 30); do
    STATUS=$(curl -sf "$API/api/scans/$SCAN_ID" | python3 -c 'import json,sys;print(json.load(sys.stdin)["state"])')
    if [[ "$STATUS" == "completed" ]]; then break; fi
    if [[ "$STATUS" == "failed" ]]; then die "scan failed"; fi
    sleep 1
done
assert_eq "$STATUS" "completed" "scan state"
note "  -> scan completed"

note "scenario 6: compatibility orphan sweep (dry-run then real)"
# The compatibility API uses a separate FID catalog. Seed its own test rows.
test_sql "$DB" <<SQL
INSERT INTO entries (fid, name, kind, size, last_seen)
SELECT fid, name, kind, size,
    IF(name = 'scan-1.txt', $SCAN_START - 1, $SCAN_START + 1)
FROM scoped_entries WHERE filesystem_id = '$FILESYSTEM' AND name IN ('scan-1.txt', 'scan-2.txt');
SQL
rm "$TEST_DIR/scan-1.txt"
# Record a dry-run count, then let the real sweep reconcile.
DRY_JSON=$(curl -sf -X POST "$API/api/compat/lustre/admin/sweep-orphans?before=$SCAN_START&dry_run=true")
DRY=$(echo "$DRY_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin)["swept"])')
assert_eq "$DRY" "1" "dry-run sweep candidates"
REAL_JSON=$(curl -sf -X POST "$API/api/compat/lustre/admin/sweep-orphans?before=$SCAN_START")
REAL=$(echo "$REAL_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin)["swept"])')
assert_eq "$REAL" "1" "real sweep candidates"

# ---- 5. admin dump / restore round-trip ----
note "scenario 7: compatibility catalog dump/restore round-trip"
DUMP=$(mktemp)
"$BIN_CLI" --api-url "$API" admin dump --out "$DUMP"
LINES=$(wc -l < "$DUMP")
assert_ge "$LINES" "1" "dump non-empty"
# Clear the compatibility fixture, then restore it.
test_sql "$DB" -e "DELETE FROM entries;"
BEFORE=$(test_sql -N -B "$DB" -e "SELECT COUNT(*) FROM entries")
"$BIN_CLI" --api-url "$API" admin restore --input "$DUMP"
AFTER=$(test_sql -N -B "$DB" -e "SELECT COUNT(*) FROM entries")
assert_eq "$AFTER" "$LINES" "restore filled rows back in"
note "  -> dump=$LINES lines, restore ok ($BEFORE → $AFTER)"
rm -f "$DUMP"

# ---- 6. policy format ----
note "scenario 8: legacy policy rejection"
LEGACY_ERROR=$(curl -sS -X POST "$API/api/policies" -H 'Content-Type: application/json' \
    -d '{"name":"e2e_purge","kind":"purge","filesystem":"lustre","trigger":"1h"}' \
    -w '\n%{http_code}')
assert_eq "${LEGACY_ERROR##*$'\n'}" "400" "legacy policy rejected"
assert_contains "$LEGACY_ERROR" "hsm.storage.io/v1alpha3" "policy format error identifies the supported version"

# ---- 7. metrics + health ----
note "scenario 9: /api/metrics exposes families"
METRICS=$(curl -sf "$API/api/metrics")
for m in rbh_catalog_entries rbh_changelog_events_total; do
    assert_contains "$METRICS" "$m" "metric $m registered"
done
note "  -> metrics ok"

# ---- 8. new reports ----
note "scenario 10: stripe-distribution + du"
STRIPE=$(curl -sf "$API/api/reports/stripe-distribution?filesystem=$FILESYSTEM")
# OK even if empty on a fresh fs — what matters is that the endpoint works.
echo "$STRIPE" | python3 -c 'import json,sys;json.load(sys.stdin)' >/dev/null

DU_FID=$(lfs path2fid "$TEST_DIR" | tr -d '[]')
DU=$(curl -sfG "$API/api/reports/du" --data-urlencode "filesystem=$FILESYSTEM" --data-urlencode "fid=$DU_FID")
TOTAL=$(echo "$DU" | python3 -c 'import json,sys;print(json.load(sys.stdin)["total_bytes"])')
note "  -> du under test_dir reports $TOTAL bytes"

# ---- 9. graceful shutdown ----
note "sending SIGTERM"
kill -TERM "$PID"
for _ in 1 2 3 4 5 6 7 8; do
    kill -0 "$PID" 2>/dev/null || break
    sleep 1
done
kill -0 "$PID" 2>/dev/null && die "daemon did not exit within 8s of SIGTERM"
note "daemon stopped cleanly"

echo "PASS: all lustre e2e scenarios"
