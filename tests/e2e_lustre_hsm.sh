#!/usr/bin/env bash
# End-to-end HSM integration test: robinhood-rs + hsm-rs + Lustre HSM
#
# Full pipeline under test:
#   1. robinhood changelog listener → catalog (MariaDB)
#   2. robinhood hsm_archive policy → lfs hsm_archive (kernel request)
#   3. hsmd (live mode) receives KUC action → hsm-plugin-terrasync archives data
#   4. CL_HSM changelog event → sm_status.hsm_state updated in catalog
#   5. robinhood hsm_release policy → lfs hsm_release
#   6. Catalog reflects released state
#   7. Manual restore via lfs hsm_restore → data back, state returns to archived
#
# Prerequisites:
#   * Lustre mount at $LUSTRE_MOUNT (default /lustre) with HSM enabled on MDT
#   * A pre-registered changelog user (e.g. cl8); pass via RBH_CHANGELOG_USER
#   * MariaDB accessible as root
#   * robinhood and rbh binaries built: cargo build --release (in robinhood-rs)
#   * hsmd and hsm-plugin-terrasync built: cargo build --release (in hsm-rs)
#
# Env knobs:
#   LUSTRE_MOUNT              — /lustre (default)
#   RBH_CHANGELOG_USER        — required (e.g. cl8)
#   RBH_MDTS                  — testfs-MDT0000 (default)
#   RBH_E2E_PORT              — 8090 (default; avoids clash with e2e_lustre.sh)
#   RBH_E2E_DB                — rbh_e2e_hsm (default)
#   RBH_E2E_CHANGELOG_SETTLE  — 10 (seconds to wait for changelog events)
#   HSM_SETTLE                — 40 (seconds to wait for HSM action completion)
#   ARCHIVE_ID                — 1 (default)
#   HSMD_BIN                  — path to hsmd binary
#   TERRASYNC_PLUGIN_BIN      — path to hsm-plugin-terrasync binary
#   RBH_E2E_KEEP_DB=1         — don't drop DB on exit (debugging)
#
# Usage:
#   RBH_CHANGELOG_USER=cl8 ./tests/e2e_lustre_hsm.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HSM_RS_ROOT="$(cd "$ROOT/../hsm-rs" && pwd 2>/dev/null)" || HSM_RS_ROOT=""

PORT=${RBH_E2E_PORT:-8090}
DB=${RBH_E2E_DB:-rbh_e2e_hsm}
API="http://127.0.0.1:$PORT"
RBH_LOG="/tmp/rbh-e2e-hsm.log"
HSMD_LOG="/tmp/rbh-e2e-hsmd.log"
PLUGIN_LOG="/tmp/rbh-e2e-terrasync.log"

BIN_DAEMON="$ROOT/target/release/robinhood"
BIN_CLI="$ROOT/target/release/rbh"
HSMD_BIN="${HSMD_BIN:-$HSM_RS_ROOT/target/release/hsmd}"
TERRASYNC_PLUGIN_BIN="${TERRASYNC_PLUGIN_BIN:-$HSM_RS_ROOT/target/release/hsm-plugin-terrasync}"

MOUNT=${LUSTRE_MOUNT:-/lustre}
MDTS=${RBH_MDTS:-testfs-MDT0000}
SETTLE=${RBH_E2E_CHANGELOG_SETTLE:-10}
HSM_SETTLE=${HSM_SETTLE:-40}
ARCHIVE_ID=${ARCHIVE_ID:-1}

RBH_PID=""
HSMD_PID=""
PLUGIN_PID=""
TEST_DIR=""
HSM_ARCHIVE_ROOT=""
HSMD_SOCK=""
HSMD_CFG=""
PLUGIN_CFG=""

cleanup() {
    for pid_var in RBH_PID HSMD_PID PLUGIN_PID; do
        local pid="${!pid_var:-}"
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            echo "[cleanup] SIGTERM ${pid_var}=$pid"
            kill -TERM "$pid" 2>/dev/null || true
            local i=0
            while kill -0 "$pid" 2>/dev/null && [[ $i -lt 8 ]]; do
                sleep 1; i=$((i + 1))
            done
            kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null || true
        fi
    done

    # restore any released files before rm so lustre doesn't complain
    if [[ -n "$TEST_DIR" && -d "$TEST_DIR" ]]; then
        local _restored=0
        for f in "$TEST_DIR"/file-*.txt; do
            if [[ -f "$f" ]]; then
                lfs hsm_restore "$f" 2>/dev/null || true
                _restored=1
            fi
        done
        # wait for restore only when files were actually released
        [[ $_restored -eq 1 ]] && sleep 4 2>/dev/null || true
        rm -rf "$TEST_DIR" 2>/dev/null || true
    fi

    [[ -n "$HSM_ARCHIVE_ROOT" ]] && rm -rf "$HSM_ARCHIVE_ROOT" 2>/dev/null || true
    [[ -n "$HSMD_CFG" ]]         && rm -f  "$HSMD_CFG"         2>/dev/null || true
    [[ -n "$PLUGIN_CFG" ]]       && rm -f  "$PLUGIN_CFG"       2>/dev/null || true
    [[ -n "$HSMD_SOCK" ]]        && rm -f  "$HSMD_SOCK"        2>/dev/null || true

    if [[ "${RBH_E2E_KEEP_DB:-0}" != "1" ]]; then
        mysql -u root -e "DROP DATABASE IF EXISTS $DB;" 2>/dev/null || true
    fi
}
trap cleanup EXIT

die()  { echo "FAIL: $*" >&2; exit 1; }
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
    [[ "$hay" == *"$needle"* ]] || die "$tag: expected to contain [$needle], got: $hay"
}

# Poll lfs hsm_state until the file state string contains $want.
wait_hsm_state() {
    local file="$1" want="$2" timeout="${3:-$HSM_SETTLE}"
    local deadline=$((SECONDS + timeout))
    while [[ $SECONDS -lt $deadline ]]; do
        local s
        s=$(lfs hsm_state "$file" 2>/dev/null || echo "")
        [[ "$s" == *"$want"* ]] && return 0
        sleep 2
    done
    local final
    final=$(lfs hsm_state "$file" 2>/dev/null || echo "unknown")
    die "wait_hsm_state: $file expected '$want' within ${timeout}s, got: $final"
}

# Poll MariaDB until sm_status.hsm_state for $name equals $want.
wait_catalog_hsm() {
    local name="$1" want="$2" timeout="${3:-$SETTLE}"
    local safe_name
    safe_name=$(printf '%s' "$name" | sed "s/'/\\\\'/g")
    local sql="SELECT JSON_UNQUOTE(JSON_EXTRACT(sm_status, '$.hsm_state')) FROM entries WHERE name='${safe_name}'"
    local deadline=$((SECONDS + timeout))
    while [[ $SECONDS -lt $deadline ]]; do
        local v
        v=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo "NULL")
        [[ "$v" == "$want" ]] && return 0
        sleep 2
    done
    local final
    final=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo "NULL")
    die "wait_catalog_hsm: $name expected hsm_state='$want' within ${timeout}s, got: $final"
}

wait_for_entry() {
    local sql="SELECT COUNT(*) FROM entries WHERE $1"
    local deadline=$((SECONDS + SETTLE))
    while [[ $SECONDS -lt $deadline ]]; do
        local n
        n=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo 0)
        [[ "$n" -gt 0 ]] && return 0
        sleep 1
    done
    die "wait_for_entry: timeout for [$1]"
}

# ---- 0. prerequisites -------------------------------------------------------
[[ -x "$BIN_DAEMON" ]]          || die "missing $BIN_DAEMON — run: cargo build --release (robinhood-rs)"
[[ -x "$BIN_CLI" ]]             || die "missing $BIN_CLI    — run: cargo build --release (robinhood-rs)"
[[ -x "$HSMD_BIN" ]]            || die "missing $HSMD_BIN   — run: cargo build --release (hsm-rs)"
[[ -x "$TERRASYNC_PLUGIN_BIN" ]]|| die "missing $TERRASYNC_PLUGIN_BIN — run: cargo build --release (hsm-rs)"
command -v mysql >/dev/null 2>&1 || die "mysql CLI not found"
[[ -d "$MOUNT" ]]                || die "Lustre mount $MOUNT does not exist"
[[ -n "${RBH_CHANGELOG_USER:-}" ]] || die "RBH_CHANGELOG_USER not set (e.g. cl8)"

# Verify HSM is enabled: lfs hsm_state must succeed on a test file
_probe=$(mktemp "$MOUNT/.hsm-probe-XXXXXX")
lfs hsm_state "$_probe" >/dev/null 2>&1 || { rm -f "$_probe"; die "lfs hsm_state failed — HSM not enabled on MDT?"; }
rm -f "$_probe"

# ---- 1. setup ---------------------------------------------------------------
TEST_DIR=$(mktemp -d "$MOUNT/rbh-hsm-e2e-XXXXXX")    || die "cannot mktemp under $MOUNT"
HSM_ARCHIVE_ROOT=$(mktemp -d "/tmp/rbh-hsm-archive-XXXXXX") || die "cannot mktemp for archive root"
HSMD_SOCK=$(mktemp -u "/tmp/hsmd-e2e-XXXXXX.sock")
HSMD_CFG=$(mktemp "/tmp/hsmd-e2e-XXXXXX.toml")
PLUGIN_CFG=$(mktemp "/tmp/terrasync-e2e-XXXXXX.toml")

note "test workspace : $TEST_DIR"
note "archive root   : $HSM_ARCHIVE_ROOT"
note "hsmd socket    : $HSMD_SOCK"

# ---- 2. fresh DB ------------------------------------------------------------
note "creating fresh DB $DB"
mysql -u root -e "DROP DATABASE IF EXISTS $DB; CREATE DATABASE $DB;" \
    || die "cannot create DB"

# ---- 3. write hsmd config ---------------------------------------------------
cat >"$HSMD_CFG" <<TOML
mode = "live"
mountpoint = "$MOUNT"
archive_ids = [$ARCHIVE_ID]

[transport]
socket_path = "$HSMD_SOCK"

[scheduler]
tick_interval_ms = 100
max_per_tick = 16

[log]
filter = "info"
format = "pretty"

[xattr]
namespace = "trusted"
TOML

# ---- 4. write terrasync plugin config ---------------------------------------
cat >"$PLUGIN_CFG" <<TOML
socket_path = "$HSMD_SOCK"
agent_id = "terrasync-e2e"
archive_ids = [$ARCHIVE_ID]
archive_root_url = "file://$HSM_ARCHIVE_ROOT"
log_filter = "info"
TOML

# ---- 5. start hsmd ----------------------------------------------------------
note "starting hsmd (live mode, archive_id=$ARCHIVE_ID)"
"$HSMD_BIN" --config "$HSMD_CFG" >"$HSMD_LOG" 2>&1 &
HSMD_PID=$!
# wait for socket to appear (up to 15s)
for _ in {1..15}; do
    [[ -S "$HSMD_SOCK" ]] && break
    kill -0 "$HSMD_PID" 2>/dev/null || {
        echo "-- hsmd log --"; cat "$HSMD_LOG"; die "hsmd exited before creating socket"
    }
    sleep 1
done
[[ -S "$HSMD_SOCK" ]] || { echo "-- hsmd log --"; cat "$HSMD_LOG"; die "hsmd socket not created within 15s"; }
note "hsmd ready (pid=$HSMD_PID)"

# ---- 6. start terrasync plugin ----------------------------------------------
note "starting hsm-plugin-terrasync"
"$TERRASYNC_PLUGIN_BIN" --config "$PLUGIN_CFG" >"$PLUGIN_LOG" 2>&1 &
PLUGIN_PID=$!
# Poll up to 15s for the plugin to produce log output (confirms it started and
# connected to hsmd rather than silently hanging before the first log line).
for _ in {1..15}; do
    kill -0 "$PLUGIN_PID" 2>/dev/null || {
        echo "-- plugin log --"; cat "$PLUGIN_LOG"; die "terrasync plugin exited immediately"
    }
    [[ -s "$PLUGIN_LOG" ]] && break
    sleep 1
done
kill -0 "$PLUGIN_PID" 2>/dev/null || {
    echo "-- plugin log --"; cat "$PLUGIN_LOG"; die "terrasync plugin exited after startup"
}
note "hsm-plugin-terrasync ready (pid=$PLUGIN_PID)"

# ---- 7. start robinhood daemon ----------------------------------------------
note "starting robinhood on :$PORT (changelog user=$RBH_CHANGELOG_USER)"
RBH_DATABASE_URL="mysql://root@127.0.0.1/$DB" \
RBH_LUSTRE_MOUNT="$MOUNT" \
RBH_MDTS="$MDTS" \
RBH_CHANGELOG_USER="$RBH_CHANGELOG_USER" \
RBH_LOG="info" \
RBH_LISTEN_ADDR="127.0.0.1:$PORT" \
    "$BIN_DAEMON" >"$RBH_LOG" 2>&1 &
RBH_PID=$!

_rbh_up=0
for _ in {1..30}; do
    if curl -sf "$API/api/health" >/dev/null 2>&1; then _rbh_up=1; break; fi
    kill -0 "$RBH_PID" 2>/dev/null || { echo "-- rbh log --"; cat "$RBH_LOG"; die "robinhood exited"; }
    sleep 1
done
[[ $_rbh_up -eq 1 ]] || { echo "-- rbh log --"; cat "$RBH_LOG"; die "robinhood health check failed within 30s"; }
note "robinhood ready (pid=$RBH_PID)"

# ---- 8. create test files ---------------------------------------------------
note "creating 3 test files in $TEST_DIR"
for i in 1 2 3; do
    dd if=/dev/urandom of="$TEST_DIR/file-$i.txt" bs=4096 count="$i" 2>/dev/null
done
note "  -> file-{1,2,3}.txt created ($(du -sh "$TEST_DIR" | cut -f1) total)"

# ---- 9. wait for catalog ----------------------------------------------------
note "waiting for changelog to catalog test files..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" \
        -e "SELECT COUNT(*) FROM entries WHERE name IN ('file-1.txt','file-2.txt','file-3.txt')" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break
    sleep 1
done
[[ "$_n" -ge 3 ]] || die "timeout waiting for all 3 test files to appear in catalog"
note "  -> all 3 files in catalog"

# Verify initial HSM state is 0x0 (no HSM state)
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/file-$i.txt" 2>/dev/null)
    assert_contains "$s" "0x00000000" "initial HSM state is none for file-$i.txt"
done
note "  -> initial HSM state verified (none)"

# ---- 10. HSM archive policy -------------------------------------------------
note "creating hsm_archive policy"
ARCHIVE_POL=$(cat <<JSON
{
  "name": "e2e_hsm_archive",
  "kind": "hsm_archive",
  "scope": {"op": "cmp", "field": "size", "cmp": "gt", "value": 0},
  "rules": [{"condition": {"op": "true"}, "action": {}}],
  "default_action": {
    "max_count": 10,
    "nb_threads": 2,
    "hsm": {"archive_id": $ARCHIVE_ID}
  },
  "triggers": []
}
JSON
)
ARCH_ID=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' -d "$ARCHIVE_POL" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> archive policy id=$ARCH_ID"

note "dry-run archive policy..."
curl -sf -X POST "$API/api/policies/$ARCH_ID/run" \
    -H 'content-type: application/json' -d '{"dry_run":true}' >/dev/null
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/file-$i.txt" 2>/dev/null)
    assert_contains "$s" "0x00000000" "dry-run left file-$i.txt unarchived"
done
note "  -> dry-run verified: no archive requests submitted"

note "running archive policy (live)..."
RUN=$(curl -sf -X POST "$API/api/policies/$ARCH_ID/run" \
    -H 'content-type: application/json' -d '{}')
note "  -> policy run: $(echo "$RUN" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(f"acted={d.get(\"acted\",\"?\")}")')"

# ---- 11. wait for hsmd + terrasync to archive -------------------------------
note "waiting for HSM archive to complete (hsmd → terrasync)..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        [[ "$(lfs hsm_state "$TEST_DIR/file-$i.txt" 2>/dev/null)" == *"archived"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break
    sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/file-$i.txt" 2>/dev/null || echo "unknown")
    [[ "$s" == *"archived"* ]] || die "file-$i.txt not archived within ${HSM_SETTLE}s, got: $s"
    note "  -> file-$i.txt: $s"
done
note "  -> all 3 files archived by hsmd + terrasync"

# Verify data was physically copied to archive root
ARCHIVED=$(find "$HSM_ARCHIVE_ROOT" -type f | wc -l)
assert_ge "$ARCHIVED" "3" "archive root has ≥ 3 files"
note "  -> archive root: $ARCHIVED files under $HSM_ARCHIVE_ROOT"

# ---- 12. wait for changelog to update catalog HSM state ---------------------
note "waiting for changelog CL_HSM events to update catalog..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" -e \
        "SELECT COUNT(*) FROM entries WHERE name IN ('file-1.txt','file-2.txt','file-3.txt') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state'))='archived'" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break
    sleep 2
done
[[ "$_n" -ge 3 ]] || die "timeout: not all 3 files have hsm_state=archived in catalog"
note "  -> catalog sm_status.hsm_state = archived for all 3 files"

# ---- 13. HSM release policy -------------------------------------------------
note "creating hsm_release policy"
RELEASE_POL=$(cat <<JSON
{
  "name": "e2e_hsm_release",
  "kind": "hsm_release",
  "scope": {"op": "hsm_state_eq", "state": "archived"},
  "rules": [{"condition": {"op": "true"}, "action": {}}],
  "default_action": {"max_count": 10, "nb_threads": 2},
  "triggers": []
}
JSON
)
REL_ID=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' -d "$RELEASE_POL" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> release policy id=$REL_ID"

note "running release policy..."
RUN2=$(curl -sf -X POST "$API/api/policies/$REL_ID/run" \
    -H 'content-type: application/json' -d '{}')
note "  -> policy run: $(echo "$RUN2" | python3 -c 'import json,sys;d=json.load(sys.stdin);print(f"acted={d.get(\"acted\",\"?\")}")')"

# ---- 14. wait for release ---------------------------------------------------
note "waiting for HSM release (kernel marks files released)..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        [[ "$(lfs hsm_state "$TEST_DIR/file-$i.txt" 2>/dev/null)" == *"released"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break
    sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/file-$i.txt" 2>/dev/null || echo "unknown")
    [[ "$s" == *"released"* ]] || die "file-$i.txt not released within ${HSM_SETTLE}s, got: $s"
    note "  -> file-$i.txt: $s"
done
note "  -> all 3 files released (data off OSTs)"

# ---- 15. wait for catalog to reflect release --------------------------------
note "waiting for changelog CL_HSM release events..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" -e \
        "SELECT COUNT(*) FROM entries WHERE name IN ('file-1.txt','file-2.txt','file-3.txt') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state'))='released'" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break
    sleep 2
done
[[ "$_n" -ge 3 ]] || die "timeout: not all 3 files have hsm_state=released in catalog"
note "  -> catalog sm_status.hsm_state = released for all 3 files"

# ---- 16. restore one file ---------------------------------------------------
note "restoring file-1.txt via lfs hsm_restore"
lfs hsm_restore "$TEST_DIR/file-1.txt" || die "lfs hsm_restore failed"
wait_hsm_state "$TEST_DIR/file-1.txt" "archived" "$HSM_SETTLE"
s=$(lfs hsm_state "$TEST_DIR/file-1.txt")
assert_contains "$s" "archived" "file-1.txt restored: archived flag present"
if [[ "$s" == *"released"* ]]; then
    die "file-1.txt should not be released after restore, got: $s"
fi
note "  -> file-1.txt restored: $s"

# data must be readable again
BYTES=$(wc -c < "$TEST_DIR/file-1.txt")
assert_ge "$BYTES" "1" "file-1.txt data readable after restore"
note "  -> file-1.txt: $BYTES bytes readable"

# ---- 17. catalog reflects restore ------------------------------------------
note "waiting for catalog to reflect restored state (hsm_state=archived)..."
wait_catalog_hsm "file-1.txt" "archived"
note "  -> file-1.txt catalog: hsm_state=archived (restored)"

# ---- 18. metrics sanity -----------------------------------------------------
note "checking /api/metrics for HSM counter families"
METRICS=$(curl -sf "$API/api/metrics")
assert_contains "$METRICS" "rbh_changelog_events_total" "metrics: changelog events"
assert_contains "$METRICS" "rbh_policy_runs_total"      "metrics: policy runs"
note "  -> metrics ok"

# ---- 19. graceful shutdown --------------------------------------------------
note "shutting down robinhood"
kill -TERM "$RBH_PID"
for _ in {1..8}; do
    kill -0 "$RBH_PID" 2>/dev/null || break; sleep 1
done
kill -0 "$RBH_PID" 2>/dev/null && die "robinhood did not exit within 8s of SIGTERM"
note "robinhood stopped cleanly"
# plugin and hsmd are stopped by the cleanup() EXIT trap

echo ""
echo "PASS: all HSM integration scenarios"
echo "  archive   — robinhood policy → lfs hsm_archive → hsmd → terrasync ✓"
echo "  catalog   — CL_HSM changelog events update sm_status.hsm_state ✓"
echo "  release   — robinhood policy → lfs hsm_release ✓"
echo "  restore   — lfs hsm_restore → data readable again ✓"
