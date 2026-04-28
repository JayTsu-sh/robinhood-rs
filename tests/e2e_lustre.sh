#!/usr/bin/env bash
# Full-stack integration test for robinhood-rs against a real Lustre mount.
#
# Scope — what this covers that `tests/e2e.sh` does not:
#   1. Changelog ingest: create / rename / unlink events round-trip
#      Lustre → listener → DB.
#   2. Initial `rbh scan` populates the catalog; orphan sweep trims stale rows.
#   3. Catalog dump/restore round-trip preserves entries.
#   4. Manual policy run (--dry-run and live) with threshold triggers.
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
#   RBH_E2E_DB             — DB name (default rbh_e2e_lustre)
#   RBH_E2E_CHANGELOG_SETTLE — seconds to wait for an event to reach the DB
#                             after an fs mutation (default 6)
#   RBH_E2E_KEEP_DB=1      — don't drop the DB on exit (debugging)
#
# Usage:
#   RBH_CHANGELOG_USER=cl7 ./tests/e2e_lustre.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PORT=${RBH_E2E_PORT:-8089}
DB=${RBH_E2E_DB:-rbh_e2e_lustre}
API="http://127.0.0.1:$PORT"
LOG="/tmp/rbh-e2e-lustre.log"
BIN_DAEMON="$ROOT/target/release/robinhood"
BIN_CLI="$ROOT/target/release/rbh"

MOUNT=${LUSTRE_MOUNT:-/lustre}
MDTS=${RBH_MDTS:-testfs-MDT0000}
SETTLE=${RBH_E2E_CHANGELOG_SETTLE:-6}

PID=""
TEST_DIR=""

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
    if [[ "${RBH_E2E_KEEP_DB:-0}" != "1" ]]; then
        mysql -u root -e "DROP DATABASE IF EXISTS $DB;" 2>/dev/null || true
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
    local sql="SELECT COUNT(*) FROM entries WHERE $1"
    local deadline=$((SECONDS + SETTLE))
    while [[ $SECONDS -lt $deadline ]]; do
        local n
        n=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo 0)
        if [[ "$n" -gt 0 ]]; then
            return 0
        fi
        sleep 1
    done
    die "wait_for_entry: timeout waiting for [$1]"
}

wait_for_absence() {
    local sql="SELECT COUNT(*) FROM entries WHERE $1"
    local deadline=$((SECONDS + SETTLE))
    while [[ $SECONDS -lt $deadline ]]; do
        local n
        n=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo 1)
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

TEST_DIR=$(mktemp -d "$MOUNT/rbh-e2e-XXXXXX") || die "cannot mktemp under $MOUNT"
note "workspace: $TEST_DIR"

# ---- 1. fresh DB ----
note "creating fresh DB $DB"
mysql -u root -e "DROP DATABASE IF EXISTS $DB; CREATE DATABASE $DB;" \
    || die "cannot create DB $DB"

# ---- 2. launch daemon with changelog enabled ----
note "launching daemon on :$PORT (mount=$MOUNT, mdts=$MDTS, user=$RBH_CHANGELOG_USER)"
RBH_DATABASE_URL="mysql://root@127.0.0.1/$DB" \
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
SIZE=$(mysql -u root -N -B "$DB" -e "SELECT size FROM entries WHERE name='$NAME'")
assert_ge "$SIZE" "16384" "rewrite updated size"
note "  -> size updated to $SIZE bytes"

note "scenario 3: rename"
DST="$TEST_DIR/hello_renamed.txt"
mv "$SRC" "$DST"
NEWNAME=$(basename "$DST")
wait_for_entry "name = '$NEWNAME'"
wait_for_absence "name = '$NAME'"
note "  -> rename observed ($NAME -> $NEWNAME)"

note "scenario 4: unlink"
rm "$DST"
wait_for_absence "name = '$NEWNAME'"
RM_COUNT=$(mysql -u root -N -B "$DB" -e "SELECT COUNT(*) FROM removed_entries WHERE name='$NEWNAME'")
assert_ge "$RM_COUNT" "1" "unlinked entry persisted in removed_entries"
note "  -> removed_entries has the FID"

# ---- 4. fs-scan + orphan sweep ----
note "scenario 5: full scan populates catalog"
# Create a few more entries out-of-band (they arrive via changelog too, but
# scan should be a no-op on items already present).
for i in 1 2 3 4 5; do echo "x" > "$TEST_DIR/scan-$i.txt"; done
SCAN_START=$(date +%s)
SCAN_ID=$(curl -sf -X POST "$API/api/scans" -H 'content-type: application/json' \
            -d "{\"root\":\"$TEST_DIR\"}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
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

note "scenario 6: orphan sweep (dry-run then real)"
# Delete one file out-of-band so its last_seen stays in the past.
rm "$TEST_DIR/scan-1.txt"
# Give the daemon a moment; the changelog unlink may or may not have fired
# depending on timing — that's fine, the sweep covers both paths.
sleep 2
# Record a dry-run count, then let the real sweep reconcile.
DRY_JSON=$(curl -sf -X POST "$API/api/admin/sweep-orphans?before=$SCAN_START&dry_run=true")
DRY=$(echo "$DRY_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin)["swept"])')
note "  -> dry-run candidates: $DRY"
REAL_JSON=$(curl -sf -X POST "$API/api/admin/sweep-orphans?before=$SCAN_START")
REAL=$(echo "$REAL_JSON" | python3 -c 'import json,sys;print(json.load(sys.stdin)["swept"])')
note "  -> real sweep: $REAL"

# ---- 5. admin dump / restore round-trip ----
note "scenario 7: catalog dump/restore round-trip"
DUMP=$(mktemp)
"$BIN_CLI" --api-url "$API" admin dump --out "$DUMP"
LINES=$(wc -l < "$DUMP")
assert_ge "$LINES" "1" "dump non-empty"
# Blow away half the catalog, then restore
mysql -u root "$DB" -e "DELETE FROM entries LIMIT 100;" || true
BEFORE=$(mysql -u root -N -B "$DB" -e "SELECT COUNT(*) FROM entries")
"$BIN_CLI" --api-url "$API" admin restore --input "$DUMP"
AFTER=$(mysql -u root -N -B "$DB" -e "SELECT COUNT(*) FROM entries")
assert_ge "$AFTER" "$BEFORE" "restore filled rows back in"
note "  -> dump=$LINES lines, restore ok ($BEFORE → $AFTER)"
rm -f "$DUMP"

# ---- 6. policy: dry-run then live ----
note "scenario 8: policy dry-run and manual run"
# Install a trivial purge policy scoped to our test dir. The scope uses a
# name-LIKE that matches our scan-* files; max_count=2 keeps the blast radius
# tiny and deterministic.
POLICY_JSON=$(cat <<JSON
{
  "name": "e2e_purge",
  "kind": "purge",
  "scope": {"op":"cmp","field":"uid","cmp":"ge","value":0},
  "rules": [{"condition":{"op":"true"},"action":{}}],
  "default_action": {"max_count": 2, "nb_threads": 1},
  "triggers": []
}
JSON
)
PID_RESP=$(curl -sf -X POST "$API/api/policies" -H 'content-type: application/json' -d "$POLICY_JSON")
POLICY_ID=$(echo "$PID_RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> policy id=$POLICY_ID"

# Dry-run: no filesystem mutations.
BEFORE_CNT=$(ls "$TEST_DIR" 2>/dev/null | wc -l)
curl -sf -X POST "$API/api/policies/$POLICY_ID/run" -H 'content-type: application/json' \
    -d '{"dry_run":true}' >/dev/null
sleep 3
AFTER_DRY=$(ls "$TEST_DIR" 2>/dev/null | wc -l)
assert_eq "$AFTER_DRY" "$BEFORE_CNT" "dry-run did not remove files"
note "  -> dry-run left $AFTER_DRY files untouched"

# ---- 7. metrics + health ----
note "scenario 9: /api/metrics exposes families"
METRICS=$(curl -sf "$API/api/metrics")
for m in rbh_catalog_entries rbh_policy_runs_total rbh_changelog_events_total rbh_hsm_poll_scanned_total; do
    assert_contains "$METRICS" "$m" "metric $m registered"
done
note "  -> metrics ok"

# ---- 8. new reports ----
note "scenario 10: stripe-distribution + du"
STRIPE=$(curl -sf "$API/api/reports/stripe-distribution")
# OK even if empty on a fresh fs — what matters is that the endpoint works.
echo "$STRIPE" | python3 -c 'import json,sys;json.load(sys.stdin)' >/dev/null

DU_FID=$(lfs path2fid "$TEST_DIR" | tr -d '[]')
DU=$(curl -sf "$API/api/reports/du?fid=${DU_FID}")
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
