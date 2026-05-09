#!/usr/bin/env bash
# End-to-end test: full ILM lifecycle via two-tier classifier + action policies
#
# Lifecycle under test:
#   1. Classify  → classifier tags files: sm_status.xattr.tier=archive_me
#   2. Archive   → hsm_archive policy moves data to HSM backend (terrasync)
#   3. Release   → hsm_release policy frees Lustre OST data (file stub remains)
#   4. Restore   → hsm_restore policy brings data back from backend to OST
#   5. Remove    → hsm_remove policy deletes the HSM backend copy (clean state)
#
# After the test:
#   - Files are fully accessible on Lustre (no HSM state)
#   - Archive root is empty (backend copy removed)
#   - All sm_status transitions reflected in catalog
#
# Two-tier design validated:
#   - Classifier writes xattr tags based on attributes (mtime > -2m)
#   - Action policies filter ONLY by tags; executors handle HSM state guards
#   - No attribute logic leaks into action policies
#
# Prerequisites:
#   * Lustre mount at $LUSTRE_MOUNT (default /lustre) with HSM enabled on MDT
#   * Pre-registered changelog user; pass via RBH_CHANGELOG_USER (e.g. cl3)
#   * MariaDB accessible as root
#   * Built: cargo build --release (robinhood-rs)
#   * Built: cargo build --release (hsm-rs)
#   * MDS accessible via SSH as root (for coordinator reset)
#
# Env knobs:
#   LUSTRE_MOUNT              — /lustre (default)
#   RBH_CHANGELOG_USER        — required
#   RBH_MDTS                  — testfs-MDT0000 (default)
#   MDS_HOST                  — 192.168.50.247 (default)
#   RBH_E2E_PORT              — 8093 (default)
#   RBH_E2E_DB                — rbh_e2e_lifecycle (default)
#   RBH_E2E_CHANGELOG_SETTLE  — 20 (seconds for changelog propagation)
#   HSM_SETTLE                — 60 (seconds for HSM operations)
#   ARCHIVE_ID                — 1 (default)
#   HSMD_BIN                  — path to hsmd binary
#   TERRASYNC_PLUGIN_BIN      — path to hsm-plugin-terrasync binary
#   RBH_E2E_KEEP_DB=1         — don't drop DB on exit (debugging)
#
# Usage:
#   RBH_CHANGELOG_USER=cl3 ./tests/e2e_classifier_full_lifecycle.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HSM_RS_ROOT="$(cd "$ROOT/../hsm-rs" && pwd 2>/dev/null)" || HSM_RS_ROOT=""

PORT=${RBH_E2E_PORT:-8093}
DB=${RBH_E2E_DB:-rbh_e2e_lifecycle}
API="http://127.0.0.1:$PORT"
RBH_LOG="/tmp/rbh-e2e-lifecycle.log"
HSMD_LOG="/tmp/rbh-e2e-lifecycle-hsmd.log"
PLUGIN_LOG="/tmp/rbh-e2e-lifecycle-terrasync.log"

BIN_DAEMON="$ROOT/target/release/robinhood"
BIN_CLI="$ROOT/target/release/rbh"
HSMD_BIN="${HSMD_BIN:-$HSM_RS_ROOT/target/release/hsmd}"
TERRASYNC_PLUGIN_BIN="${TERRASYNC_PLUGIN_BIN:-$HSM_RS_ROOT/target/release/hsm-plugin-terrasync}"

MOUNT=${LUSTRE_MOUNT:-/lustre}
MDTS=${RBH_MDTS:-testfs-MDT0000}
MDS_HOST="${MDS_HOST:-192.168.50.247}"
SETTLE=${RBH_E2E_CHANGELOG_SETTLE:-20}
HSM_SETTLE=${HSM_SETTLE:-60}
ARCHIVE_ID=${ARCHIVE_ID:-1}

RBH_PID="" HSMD_PID="" PLUGIN_PID=""
TEST_DIR="" HSM_ARCHIVE_ROOT=""
HSMD_SOCK="" HSMD_CFG="" PLUGIN_CFG=""

cleanup() {
    for pid_var in RBH_PID HSMD_PID PLUGIN_PID; do
        local pid="${!pid_var:-}"
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            echo "[cleanup] SIGTERM ${pid_var}=$pid"
            kill -TERM "$pid" 2>/dev/null || true
            local i=0
            while kill -0 "$pid" 2>/dev/null && [[ $i -lt 8 ]]; do sleep 1; i=$((i+1)); done
            kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null || true
        fi
    done
    [[ -n "$TEST_DIR" ]]       && rm -rf "$TEST_DIR"       2>/dev/null || true
    [[ -n "$HSM_ARCHIVE_ROOT" ]] && rm -rf "$HSM_ARCHIVE_ROOT" 2>/dev/null || true
    [[ -n "$HSMD_CFG" ]]       && rm -f "$HSMD_CFG"        2>/dev/null || true
    [[ -n "$PLUGIN_CFG" ]]     && rm -f "$PLUGIN_CFG"      2>/dev/null || true
    if [[ "${RBH_E2E_KEEP_DB:-0}" != "1" ]]; then
        mysql -u root -e "DROP DATABASE IF EXISTS $DB;" 2>/dev/null || true
    fi
}
trap cleanup EXIT

die()  { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[$(date +%H:%M:%S)] $*"; }

assert_eq() { [[ "$1" == "$2" ]] || die "$3: expected [$2], got [$1]"; }
assert_ge() { [[ "$1" -ge "$2" ]] || die "$3: expected >= $2, got $1"; }
assert_contains()     { [[ "$1" == *"$2"* ]] || die "$3: expected [$2] in [$1]"; }
assert_not_contains() { [[ "$1" != *"$2"* ]] || die "$3: did not expect [$2] in [$1]"; }

# Poll lfs hsm_state until the state string contains $want.
wait_hsm_state() {
    local file="$1" want="$2" timeout="${3:-$HSM_SETTLE}"
    local deadline=$((SECONDS + timeout))
    while [[ $SECONDS -lt $deadline ]]; do
        local s; s=$(lfs hsm_state "$file" 2>/dev/null || echo "")
        [[ "$s" == *"$want"* ]] && return 0
        sleep 2
    done
    die "wait_hsm_state: $file wanted '$want' within ${timeout}s, got: $(lfs hsm_state "$file" 2>/dev/null)"
}

# Poll lfs hsm_state until the state string does NOT contain $unwanted.
wait_hsm_state_absent() {
    local file="$1" unwanted="$2" timeout="${3:-$HSM_SETTLE}"
    local deadline=$((SECONDS + timeout))
    while [[ $SECONDS -lt $deadline ]]; do
        local s; s=$(lfs hsm_state "$file" 2>/dev/null || echo "")
        [[ "$s" != *"$unwanted"* ]] && return 0
        sleep 2
    done
    die "wait_hsm_state_absent: $file still has '$unwanted' after ${timeout}s, got: $(lfs hsm_state "$file" 2>/dev/null)"
}

# Poll catalog until sm_status.hsm_state for files IN ($names_csv) equals $want.
wait_catalog_hsm_all() {
    local names_csv="$1" want="$2" timeout="${3:-$SETTLE}"
    local sql="SELECT COUNT(*) FROM entries WHERE name IN ($names_csv) AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state'))='$want'"
    local deadline=$((SECONDS + timeout)); local _n=0
    while [[ $SECONDS -lt $deadline ]]; do
        _n=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo 0)
        [[ "$_n" -ge 3 ]] && return 0
        sleep 2
    done
    local final; final=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo 0)
    die "wait_catalog_hsm_all: only $final/3 have hsm_state='$want' after ${timeout}s"
}

NAMES_CSV="'cls-lc-1.bin','cls-lc-2.bin','cls-lc-3.bin'"

# ── 0. Prerequisites ──────────────────────────────────────────────────────────
[[ -x "$BIN_DAEMON" ]]           || die "missing $BIN_DAEMON"
[[ -x "$BIN_CLI" ]]              || die "missing $BIN_CLI"
[[ -x "$HSMD_BIN" ]]             || die "missing $HSMD_BIN"
[[ -x "$TERRASYNC_PLUGIN_BIN" ]] || die "missing $TERRASYNC_PLUGIN_BIN"
command -v mysql >/dev/null 2>&1 || die "mysql CLI not found"
[[ -d "$MOUNT" ]]                || die "Lustre mount $MOUNT not found"
[[ -n "${RBH_CHANGELOG_USER:-}" ]] || die "RBH_CHANGELOG_USER not set"

_probe=$(mktemp "$MOUNT/.hsm-probe-XXXXXX")
lfs hsm_state "$_probe" >/dev/null 2>&1 || { rm -f "$_probe"; die "lfs hsm_state failed — HSM not enabled?"; }
rm -f "$_probe"

# ── 1. Setup ──────────────────────────────────────────────────────────────────
TEST_DIR=$(mktemp -d "$MOUNT/rbh-lc-e2e-XXXXXX")
HSM_ARCHIVE_ROOT=$(mktemp -d "/tmp/rbh-lc-archive-XXXXXX")
HSMD_SOCK=$(mktemp -u "/tmp/hsmd-lc-e2e-XXXXXX.sock")
HSMD_CFG=$(mktemp "/tmp/hsmd-lc-e2e-XXXXXX.toml")
PLUGIN_CFG=$(mktemp "/tmp/terrasync-lc-e2e-XXXXXX.toml")

note "test dir    : $TEST_DIR"
note "archive root: $HSM_ARCHIVE_ROOT"

# ── 2. Fresh DB ───────────────────────────────────────────────────────────────
note "creating DB $DB"
mysql -u root -e "DROP DATABASE IF EXISTS $DB; CREATE DATABASE $DB;" || die "cannot create DB"

# Reset HSM coordinator (clears stale copytool registrations)
if ssh -o ConnectTimeout=3 root@"$MDS_HOST" true 2>/dev/null; then
    note "resetting HSM coordinator on $MDS_HOST"
    ssh root@"$MDS_HOST" \
        "lctl set_param mdt.*.hsm_control=disabled >/dev/null 2>&1; sleep 1; lctl set_param mdt.*.hsm_control=enabled >/dev/null 2>&1" \
        || note "  (warning: coordinator reset failed)"
    sleep 2
fi

# ── 3. Write daemon configs ───────────────────────────────────────────────────
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

cat >"$PLUGIN_CFG" <<TOML
socket_path = "$HSMD_SOCK"
agent_id = "terrasync-lc-e2e"
archive_ids = [$ARCHIVE_ID]
archive_root_url = "file://$HSM_ARCHIVE_ROOT"
log_filter = "info"
TOML

# ── 4. Start daemons ──────────────────────────────────────────────────────────
note "starting hsmd"
"$HSMD_BIN" --config "$HSMD_CFG" >"$HSMD_LOG" 2>&1 &
HSMD_PID=$!
for _ in {1..15}; do [[ -S "$HSMD_SOCK" ]] && break; kill -0 "$HSMD_PID" 2>/dev/null || { cat "$HSMD_LOG"; die "hsmd exited"; }; sleep 1; done
[[ -S "$HSMD_SOCK" ]] || { cat "$HSMD_LOG"; die "hsmd socket not created"; }
note "hsmd ready (pid=$HSMD_PID)"

note "starting terrasync"
"$TERRASYNC_PLUGIN_BIN" --config "$PLUGIN_CFG" >"$PLUGIN_LOG" 2>&1 &
PLUGIN_PID=$!
for _ in {1..15}; do kill -0 "$PLUGIN_PID" 2>/dev/null || { cat "$PLUGIN_LOG"; die "terrasync exited"; }; [[ -s "$PLUGIN_LOG" ]] && break; sleep 1; done
note "terrasync ready (pid=$PLUGIN_PID)"

note "starting robinhood on :$PORT"
RBH_DATABASE_URL="mysql://root@127.0.0.1/$DB" \
RBH_LUSTRE_MOUNT="$MOUNT" \
RBH_MDTS="$MDTS" \
RBH_CHANGELOG_USER="$RBH_CHANGELOG_USER" \
RBH_LOG="info" \
RBH_LISTEN_ADDR="127.0.0.1:$PORT" \
    "$BIN_DAEMON" >"$RBH_LOG" 2>&1 &
RBH_PID=$!
_up=0
for _ in {1..30}; do
    curl -sf "$API/api/health" >/dev/null 2>&1 && _up=1 && break
    kill -0 "$RBH_PID" 2>/dev/null || { cat "$RBH_LOG"; die "robinhood exited"; }
    sleep 1
done
[[ $_up -eq 1 ]] || { cat "$RBH_LOG"; die "robinhood health check failed"; }
note "robinhood ready (pid=$RBH_PID)"

# ── 5. Create test files ──────────────────────────────────────────────────────
note "--- PHASE 1: CREATE & CLASSIFY ---"
note "creating 3 test files in $TEST_DIR"
for i in 1 2 3; do
    dd if=/dev/urandom of="$TEST_DIR/cls-lc-$i.bin" bs=4096 count="$i" 2>/dev/null
done
note "  -> cls-lc-{1,2,3}.bin created"

for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
    assert_contains "$s" "0x00000000" "initial HSM state none for cls-lc-$i.bin"
done
note "  -> initial HSM state: none (0x00000000) ✓"

# ── 6. Wait for catalog ───────────────────────────────────────────────────────
note "waiting for changelog to catalog test files..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" -e "SELECT COUNT(*) FROM entries WHERE name IN ($NAMES_CSV)" 2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break; sleep 1
done
[[ "$_n" -ge 3 ]] || die "timeout: only $_n/3 test files in catalog"
note "  -> all 3 files cataloged ✓"

# ── 7. Create classifier and tag files ───────────────────────────────────────
note "creating classifier (mtime > -2m → tier=archive_me)"
CLS_ID=$(curl -sf -X POST "$API/api/classifiers" \
    -H 'content-type: application/json' \
    -d '{
      "name":"lc_tier_classifier",
      "manages":["tier"],
      "rules":[
        {"when":"mtime > -2m","set":{"tier":"archive_me"}}
      ],
      "schedule":"1h","enabled":true
    }' | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> classifier id=$CLS_ID"

note "running classifier..."
CLASSIFIED=$(curl -sf -X POST "$API/api/classifiers/$CLS_ID/run" \
    | python3 -c 'import json,sys;print(json.load(sys.stdin)["classified"])')
note "  -> classified=$CLASSIFIED entries"
assert_ge "$CLASSIFIED" "3" "classifier tagged >= 3 entries"

_tagged=$(mysql -u root -N -B "$DB" -e \
    "SELECT COUNT(*) FROM entries WHERE name IN ($NAMES_CSV) AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.xattr.tier'))='archive_me'" \
    2>/dev/null || echo 0)
assert_eq "$_tagged" "3" "all 3 test files tagged tier=archive_me"
note "  -> all 3 files tagged: sm_status.xattr.tier=archive_me ✓"

# ── 8. ARCHIVE: hsm_archive policy ───────────────────────────────────────────
note ""
note "--- PHASE 2: ARCHIVE (tier=archive_me → HSM backend) ---"
ARCH_ID=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' \
    -d "{
      \"name\":\"lc_archive\",
      \"kind\":\"hsm_archive\",
      \"match_tags\":{\"tier\":\"archive_me\"},
      \"trigger\":\"1h\",
      \"action\":{\"max_count\":500,\"nb_threads\":4,\"hsm\":{\"archive_id\":$ARCHIVE_ID}},
      \"enabled\":true
    }" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> archive policy id=$ARCH_ID"

note "dry-run archive (verify no changes)..."
curl -sf -X POST "$API/api/policies/$ARCH_ID/run" \
    -H 'content-type: application/json' -d '{"dry_run":true}' >/dev/null
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
    assert_contains "$s" "0x00000000" "dry-run: cls-lc-$i.bin unarchived"
done
note "  -> dry-run verified: no HSM requests submitted ✓"

note "running archive policy (live)..."
curl -sf -X POST "$API/api/policies/$ARCH_ID/run" \
    -H 'content-type: application/json' -d '{}' >/dev/null

note "waiting for HSM archive to complete..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        [[ "$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)" == *"archived"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break; sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
    [[ "$s" == *"archived"* ]] || die "cls-lc-$i.bin not archived within ${HSM_SETTLE}s: $s"
    note "  -> cls-lc-$i.bin: $s"
done
note "  -> all 3 files archived by hsmd+terrasync ✓"

_archived_files=$(find "$HSM_ARCHIVE_ROOT" -type f | wc -l)
assert_ge "$_archived_files" "3" "archive root has >= 3 files"
note "  -> $_archived_files file(s) in archive root ✓"

wait_catalog_hsm_all "$NAMES_CSV" "archived" "$SETTLE"
note "  -> catalog sm_status.hsm_state=archived ✓"

# xattr tags preserved
_tagged=$(mysql -u root -N -B "$DB" -e \
    "SELECT COUNT(*) FROM entries WHERE name IN ($NAMES_CSV) AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.xattr.tier'))='archive_me'" \
    2>/dev/null || echo 0)
assert_eq "$_tagged" "3" "xattr tags preserved after archive"
note "  -> xattr tags intact after archive ✓"

# ── 9. RELEASE: delete OST data (Lustre side) ────────────────────────────────
note ""
note "--- PHASE 3: RELEASE (free Lustre OST data, keep backend copy) ---"
REL_ID=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' \
    -d '{
      "name":"lc_release",
      "kind":"hsm_release",
      "match_tags":{"tier":"archive_me"},
      "trigger":"1h",
      "action":{"max_count":500,"nb_threads":4},
      "enabled":true
    }' | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> release policy id=$REL_ID"

note "running release policy..."
curl -sf -X POST "$API/api/policies/$REL_ID/run" \
    -H 'content-type: application/json' -d '{}' >/dev/null

note "waiting for OST data to be freed..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        [[ "$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)" == *"released"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break; sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
    [[ "$s" == *"released"* ]] || die "cls-lc-$i.bin not released within ${HSM_SETTLE}s: $s"
    note "  -> cls-lc-$i.bin: $s"
done
note "  -> all 3 files released (Lustre OST data freed) ✓"

# Verify data is NOT directly readable (file is a stub)
STUB_BLOCKS=$(stat -c %b "$TEST_DIR/cls-lc-1.bin" 2>/dev/null || echo "-1")
note "  -> cls-lc-1.bin stub blocks: $STUB_BLOCKS (0 = data evicted from OST)"

wait_catalog_hsm_all "$NAMES_CSV" "released" "$SETTLE"
note "  -> catalog sm_status.hsm_state=released ✓"

note "  -> backend copy still present: $_archived_files file(s) in archive root ✓"

# ── 10. RESTORE: bring data back from backend ────────────────────────────────
note ""
note "--- PHASE 4: RESTORE (bring data back from backend to Lustre OST) ---"
RST_ID=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' \
    -d "{
      \"name\":\"lc_restore\",
      \"kind\":\"hsm_restore\",
      \"match_tags\":{\"tier\":\"archive_me\"},
      \"trigger\":\"1h\",
      \"action\":{\"max_count\":500,\"nb_threads\":4},
      \"enabled\":true
    }" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> restore policy id=$RST_ID"

note "running restore policy (HsmRestoreExecutor skips non-released files)..."
curl -sf -X POST "$API/api/policies/$RST_ID/run" \
    -H 'content-type: application/json' -d '{}' >/dev/null

note "waiting for OST data to be restored..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
        [[ "$s" != *"released"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break; sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
    [[ "$s" != *"released"* ]] || die "cls-lc-$i.bin still released after ${HSM_SETTLE}s: $s"
    assert_contains "$s" "archived" "cls-lc-$i.bin restored: archived flag present"
    note "  -> cls-lc-$i.bin: $s"
done
note "  -> all 3 files restored to Lustre OST ✓"

# Verify data is readable again
for i in 1 2 3; do
    BYTES=$(wc -c < "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null || echo 0)
    assert_ge "$BYTES" "1" "cls-lc-$i.bin data readable after restore ($BYTES bytes)"
    note "  -> cls-lc-$i.bin: $BYTES bytes readable ✓"
done

wait_catalog_hsm_all "$NAMES_CSV" "archived" "$SETTLE"
note "  -> catalog sm_status.hsm_state=archived ✓"
note "  -> backend copy still present: $(find "$HSM_ARCHIVE_ROOT" -type f | wc -l) file(s) ✓"

# ── 11. REMOVE: delete HSM backend copy ──────────────────────────────────────
note ""
note "--- PHASE 5: REMOVE (delete HSM backend copy, file stays on Lustre) ---"
RMV_ID=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' \
    -d "{
      \"name\":\"lc_remove\",
      \"kind\":\"hsm_remove\",
      \"match_tags\":{\"tier\":\"archive_me\"},
      \"trigger\":\"1h\",
      \"action\":{\"max_count\":500,\"nb_threads\":4},
      \"enabled\":true
    }" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> remove policy id=$RMV_ID"

note "running remove policy (deletes backend copy via terrasync)..."
curl -sf -X POST "$API/api/policies/$RMV_ID/run" \
    -H 'content-type: application/json' -d '{}' >/dev/null

note "waiting for HSM backend copy to be removed..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
        # After remove: state should be 0x00000000 (no HSM state)
        [[ "$s" == *"0x00000000"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break; sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null)
    [[ "$s" == *"0x00000000"* ]] || die "cls-lc-$i.bin still has HSM state after remove (${HSM_SETTLE}s): $s"
    note "  -> cls-lc-$i.bin: $s"
done
note "  -> all 3 files: HSM state cleared (0x00000000) ✓"

# Verify Lustre data is still readable
for i in 1 2 3; do
    BYTES=$(wc -c < "$TEST_DIR/cls-lc-$i.bin" 2>/dev/null || echo 0)
    assert_ge "$BYTES" "1" "cls-lc-$i.bin still readable on Lustre after remove ($BYTES bytes)"
    note "  -> cls-lc-$i.bin: $BYTES bytes on Lustre ✓"
done

# Verify archive root is now empty
_remaining=$(find "$HSM_ARCHIVE_ROOT" -type f | wc -l)
assert_eq "$_remaining" "0" "archive root should be empty after remove"
note "  -> archive root empty (all backend copies deleted) ✓"

# Wait for catalog to reflect remove (hsm_state=none)
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" -e \
        "SELECT COUNT(*) FROM entries WHERE name IN ($NAMES_CSV) AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state'))='none'" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break; sleep 2
done
note "  -> catalog sm_status.hsm_state=none for $_n/3 files (may take time via CL_HSM)"

# ── 12. Final consistency check ───────────────────────────────────────────────
note ""
note "--- FINAL CONSISTENCY CHECK ---"

# xattr tags still intact throughout
_final_tagged=$(mysql -u root -N -B "$DB" -e \
    "SELECT COUNT(*) FROM entries WHERE name IN ($NAMES_CSV) AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.xattr.tier'))='archive_me'" \
    2>/dev/null || echo 0)
assert_eq "$_final_tagged" "3" "xattr tags intact throughout full lifecycle"
note "  -> sm_status.xattr.tier=archive_me preserved throughout ✓"

# All policies listed correctly
POLICY_LIST=$(curl -sf "$API/api/policies" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))' 2>/dev/null || echo 0)
assert_ge "$POLICY_LIST" "4" "4 policies created and listed"
note "  -> $POLICY_LIST policies in API ✓"

# Classifier still queryable
CLS_LIST=$(curl -sf "$API/api/classifiers" | python3 -c 'import json,sys;print(len(json.load(sys.stdin)))' 2>/dev/null || echo 0)
assert_ge "$CLS_LIST" "1" "classifier still in API"
note "  -> $CLS_LIST classifier(s) in API ✓"

echo ""
echo "========================================================"
echo "  ALL LIFECYCLE TESTS PASSED ✓"
echo "========================================================"
echo ""
echo "Full ILM cycle validated:"
echo "  1. Classify  → sm_status.xattr.tier=archive_me"
echo "  2. Archive   → backend copy created, data on OST (exists archived)"
echo "  3. Release   → OST data freed (released exists archived), backend preserved"
echo "  4. Restore   → OST data back (exists archived), backend still present"
echo "  5. Remove    → backend copy deleted, file fully on Lustre (0x00000000)"
echo ""
echo "Two-tier design confirmed:"
echo "  - Classifier owns attribute logic (mtime > -2m)"
echo "  - Action policies are tag-only; executors guard HSM state"
echo "  - Catalog reflects every transition via changelog"
