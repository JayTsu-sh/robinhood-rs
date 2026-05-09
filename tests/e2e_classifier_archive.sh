#!/usr/bin/env bash
# End-to-end test: two-tier classification + HSM archive pipeline
#
# What this validates:
#   1. Classifier (POST /api/classifiers) tags files via sm_status.xattr
#   2. Action policy (POST /api/policies) reads tags and submits HSM archive
#   3. hsmd + terrasync physically archive the files
#   4. Changelog events update sm_status.hsm_state in the catalog
#   5. Release policy reads archived tag + hsm state → releases OST data
#
# Pipeline:
#   files created on Lustre
#     → changelog → catalog (entries table)
#     → classifier /run → sm_status.xattr.tier = "archive_me"
#     → archive policy (match_tags: tier=archive_me) → HSM archive request
#     → hsmd + terrasync archive → CL_HSM event → sm_status.hsm_state = archived
#     → release policy (match_tags: tier=archive_me) → HSM release request
#     → sm_status.hsm_state = released
#
# Prerequisites:
#   * Lustre mount at $LUSTRE_MOUNT (default /lustre) with HSM enabled
#   * Pre-registered changelog user; pass via RBH_CHANGELOG_USER (e.g. cl8)
#   * MariaDB accessible as root
#   * Built: cargo build --release (robinhood-rs)
#   * Built: cargo build --release (hsm-rs)
#
# Env knobs:
#   LUSTRE_MOUNT              — /lustre (default)
#   RBH_CHANGELOG_USER        — required
#   RBH_MDTS                  — testfs-MDT0000 (default)
#   RBH_E2E_PORT              — 8092 (default)
#   RBH_E2E_DB                — rbh_e2e_classifier (default)
#   RBH_E2E_CHANGELOG_SETTLE  — 15 (seconds for changelog propagation)
#   HSM_SETTLE                — 60 (seconds for HSM operations)
#   ARCHIVE_ID                — 1 (default)
#   HSMD_BIN                  — path to hsmd binary
#   TERRASYNC_PLUGIN_BIN      — path to hsm-plugin-terrasync binary
#   RBH_E2E_KEEP_DB=1         — don't drop DB on exit (debugging)
#
# Usage:
#   RBH_CHANGELOG_USER=cl8 ./tests/e2e_classifier_archive.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HSM_RS_ROOT="$(cd "$ROOT/../hsm-rs" && pwd 2>/dev/null)" || HSM_RS_ROOT=""

PORT=${RBH_E2E_PORT:-8092}
DB=${RBH_E2E_DB:-rbh_e2e_classifier}
API="http://127.0.0.1:$PORT"
RBH_LOG="/tmp/rbh-e2e-classifier.log"
HSMD_LOG="/tmp/rbh-e2e-classifier-hsmd.log"
PLUGIN_LOG="/tmp/rbh-e2e-classifier-terrasync.log"

BIN_DAEMON="$ROOT/target/release/robinhood"
BIN_CLI="$ROOT/target/release/rbh"
HSMD_BIN="${HSMD_BIN:-$HSM_RS_ROOT/target/release/hsmd}"
TERRASYNC_PLUGIN_BIN="${TERRASYNC_PLUGIN_BIN:-$HSM_RS_ROOT/target/release/hsm-plugin-terrasync}"

MOUNT=${LUSTRE_MOUNT:-/lustre}
MDTS=${RBH_MDTS:-testfs-MDT0000}
SETTLE=${RBH_E2E_CHANGELOG_SETTLE:-15}
HSM_SETTLE=${HSM_SETTLE:-60}
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
    [[ -n "$TEST_DIR" ]] && rm -rf "$TEST_DIR" 2>/dev/null || true
    [[ -n "$HSM_ARCHIVE_ROOT" ]] && rm -rf "$HSM_ARCHIVE_ROOT" 2>/dev/null || true
    [[ -n "$HSMD_CFG" ]] && rm -f "$HSMD_CFG" 2>/dev/null || true
    [[ -n "$PLUGIN_CFG" ]] && rm -f "$PLUGIN_CFG" 2>/dev/null || true
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
    [[ "$hay" == *"$needle"* ]] || die "$tag: expected [$needle] in [$hay]"
}
assert_not_contains() {
    local hay="$1" needle="$2" tag="$3"
    [[ "$hay" != *"$needle"* ]] || die "$tag: did not expect [$needle] in [$hay]"
}

# Poll lfs hsm_state until the state string contains $want.
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
    die "wait_hsm_state: $file wanted '$want' within ${timeout}s, got: $final"
}

# Poll catalog until sm_status.hsm_state for filename $name equals $want.
wait_catalog_hsm() {
    local name="$1" want="$2" timeout="${3:-$SETTLE}"
    local sql="SELECT JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state')) FROM entries WHERE name='$name'"
    local deadline=$((SECONDS + timeout))
    while [[ $SECONDS -lt $deadline ]]; do
        local v
        v=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo "NULL")
        [[ "$v" == "$want" ]] && return 0
        sleep 2
    done
    local final
    final=$(mysql -u root -N -B "$DB" -e "$sql" 2>/dev/null || echo "NULL")
    die "wait_catalog_hsm: $name wanted hsm_state='$want' within ${timeout}s, got: $final"
}

# Poll until COUNT(*) WHERE $condition > 0.
wait_for_entry() {
    local cond="$1" timeout="${2:-$SETTLE}"
    local deadline=$((SECONDS + timeout))
    while [[ $SECONDS -lt $deadline ]]; do
        local n
        n=$(mysql -u root -N -B "$DB" -e "SELECT COUNT(*) FROM entries WHERE $cond" 2>/dev/null || echo 0)
        [[ "$n" -gt 0 ]] && return 0
        sleep 1
    done
    die "wait_for_entry timeout: [$cond]"
}

# ── 0. Prerequisites ──────────────────────────────────────────────────────────
[[ -x "$BIN_DAEMON" ]]           || die "missing $BIN_DAEMON — run: cargo build --release"
[[ -x "$BIN_CLI" ]]              || die "missing $BIN_CLI — run: cargo build --release"
[[ -x "$HSMD_BIN" ]]             || die "missing $HSMD_BIN — run: cargo build --release (hsm-rs)"
[[ -x "$TERRASYNC_PLUGIN_BIN" ]] || die "missing $TERRASYNC_PLUGIN_BIN — run: cargo build --release (hsm-rs)"
command -v mysql >/dev/null 2>&1 || die "mysql CLI not found"
[[ -d "$MOUNT" ]]                || die "Lustre mount $MOUNT not found"
[[ -n "${RBH_CHANGELOG_USER:-}" ]] || die "RBH_CHANGELOG_USER not set (e.g. cl8)"

_probe=$(mktemp "$MOUNT/.hsm-probe-XXXXXX")
lfs hsm_state "$_probe" >/dev/null 2>&1 || { rm -f "$_probe"; die "lfs hsm_state failed — HSM not enabled on MDT?"; }
rm -f "$_probe"

# ── 1. Setup ──────────────────────────────────────────────────────────────────
TEST_DIR=$(mktemp -d "$MOUNT/rbh-cls-e2e-XXXXXX")
HSM_ARCHIVE_ROOT=$(mktemp -d "/tmp/rbh-cls-archive-XXXXXX")
HSMD_SOCK=$(mktemp -u "/tmp/hsmd-cls-e2e-XXXXXX.sock")
HSMD_CFG=$(mktemp "/tmp/hsmd-cls-e2e-XXXXXX.toml")
PLUGIN_CFG=$(mktemp "/tmp/terrasync-cls-e2e-XXXXXX.toml")

note "test dir       : $TEST_DIR"
note "archive root   : $HSM_ARCHIVE_ROOT"

# ── 2. Fresh DB ───────────────────────────────────────────────────────────────
note "creating DB $DB"
mysql -u root -e "DROP DATABASE IF EXISTS $DB; CREATE DATABASE $DB;" || die "cannot create DB"

# Reset HSM coordinator via MDS SSH if accessible — clears stale registrations
# that accumulate from previous test runs and block new copytool registrations.
MDS_HOST="${MDS_HOST:-192.168.50.247}"
if ssh -o ConnectTimeout=3 root@"$MDS_HOST" true 2>/dev/null; then
    note "resetting HSM coordinator on $MDS_HOST (clears stale agent registrations)"
    ssh root@"$MDS_HOST" \
        "lctl set_param mdt.*.hsm_control=disabled >/dev/null 2>&1; sleep 1; lctl set_param mdt.*.hsm_control=enabled >/dev/null 2>&1" \
        || note "  (warning: coordinator reset failed — continuing)"
    sleep 2
else
    note "MDS $MDS_HOST not reachable via SSH — skipping coordinator reset"
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
agent_id = "terrasync-cls-e2e"
archive_ids = [$ARCHIVE_ID]
archive_root_url = "file://$HSM_ARCHIVE_ROOT"
log_filter = "info"
TOML

# ── 4. Start hsmd ─────────────────────────────────────────────────────────────
note "starting hsmd (archive_id=$ARCHIVE_ID)"
"$HSMD_BIN" --config "$HSMD_CFG" >"$HSMD_LOG" 2>&1 &
HSMD_PID=$!
for _ in {1..15}; do
    [[ -S "$HSMD_SOCK" ]] && break
    kill -0 "$HSMD_PID" 2>/dev/null || { cat "$HSMD_LOG"; die "hsmd exited before socket"; }
    sleep 1
done
[[ -S "$HSMD_SOCK" ]] || { cat "$HSMD_LOG"; die "hsmd socket not created within 15s"; }
note "hsmd ready (pid=$HSMD_PID)"

# ── 5. Start terrasync plugin ─────────────────────────────────────────────────
note "starting hsm-plugin-terrasync"
"$TERRASYNC_PLUGIN_BIN" --config "$PLUGIN_CFG" >"$PLUGIN_LOG" 2>&1 &
PLUGIN_PID=$!
for _ in {1..15}; do
    kill -0 "$PLUGIN_PID" 2>/dev/null || { cat "$PLUGIN_LOG"; die "terrasync exited immediately"; }
    [[ -s "$PLUGIN_LOG" ]] && break
    sleep 1
done
kill -0 "$PLUGIN_PID" 2>/dev/null || { cat "$PLUGIN_LOG"; die "terrasync exited after startup"; }
note "terrasync ready (pid=$PLUGIN_PID)"

# ── 6. Start robinhood daemon ─────────────────────────────────────────────────
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
    kill -0 "$RBH_PID" 2>/dev/null || { cat "$RBH_LOG"; die "robinhood exited during startup"; }
    sleep 1
done
[[ $_rbh_up -eq 1 ]] || { cat "$RBH_LOG"; die "robinhood health check failed within 30s"; }
note "robinhood ready (pid=$RBH_PID)"

# ── 7. Create test files ──────────────────────────────────────────────────────
note "creating 3 test files in $TEST_DIR"
for i in 1 2 3; do
    dd if=/dev/urandom of="$TEST_DIR/cls-file-$i.bin" bs=4096 count="$i" 2>/dev/null
done
note "  -> cls-file-{1,2,3}.bin created"

# Verify no HSM state yet
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-file-$i.bin" 2>/dev/null)
    assert_contains "$s" "0x00000000" "initial HSM state none for cls-file-$i.bin"
done
note "  -> initial HSM state: none"

# ── 8. Wait for catalog ───────────────────────────────────────────────────────
note "waiting for changelog to catalog all 3 test files..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" \
        -e "SELECT COUNT(*) FROM entries WHERE name IN ('cls-file-1.bin','cls-file-2.bin','cls-file-3.bin')" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break
    sleep 1
done
[[ "$_n" -ge 3 ]] || die "timeout: only $_n/3 test files in catalog after ${SETTLE}s"
note "  -> all 3 files cataloged"

# Verify no xattr tags yet
_tagged=$(mysql -u root -N -B "$DB" \
    -e "SELECT COUNT(*) FROM entries WHERE name LIKE 'cls-file-%.bin' AND JSON_EXTRACT(sm_status,'$.xattr') IS NOT NULL AND JSON_EXTRACT(sm_status,'$.xattr') != '{}'" \
    2>/dev/null || echo 0)
assert_eq "$_tagged" "0" "no xattr tags before classifier runs"
note "  -> verified: no xattr tags yet"

# ── 9. Create classifier ──────────────────────────────────────────────────────
note "creating tier classifier via POST /api/classifiers"
# Use mtime > -2m so only files created/modified in the last 2 minutes get tagged.
# This targets only our 3 test files, skipping the ~185 pre-existing catalog entries.
CLS_BODY=$(cat <<JSON
{
  "name": "e2e_tier_classifier",
  "manages": ["tier"],
  "rules": [
    {
      "when": "mtime > -2m",
      "set": {"tier": "archive_me"}
    }
  ],
  "schedule": "1h",
  "enabled": true
}
JSON
)
CLS_RESP=$(curl -sf -X POST "$API/api/classifiers" \
    -H 'content-type: application/json' \
    -d "$CLS_BODY")
CLS_ID=$(echo "$CLS_RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> classifier id=$CLS_ID"

# Verify it is stored
CLS_SHOW=$(curl -sf "$API/api/classifiers/$CLS_ID")
assert_contains "$CLS_SHOW" "archive_me" "classifier stored with correct tag"
note "  -> classifier verified via GET /api/classifiers/$CLS_ID"

# ── 10. Run classifier manually ───────────────────────────────────────────────
note "running classifier via POST /api/classifiers/$CLS_ID/run"
RUN_RESP=$(curl -sf -X POST "$API/api/classifiers/$CLS_ID/run")
CLASSIFIED=$(echo "$RUN_RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["classified"])')
note "  -> classifier run: classified=$CLASSIFIED entries"
assert_ge "$CLASSIFIED" "3" "classifier should process all 3 test files"

# ── 11. Verify tags in catalog ────────────────────────────────────────────────
note "verifying sm_status.xattr.tier='archive_me' in catalog..."
_deadline=$((SECONDS + SETTLE)); _tagged=0
while [[ $SECONDS -lt $_deadline ]]; do
    _tagged=$(mysql -u root -N -B "$DB" \
        -e "SELECT COUNT(*) FROM entries WHERE name IN ('cls-file-1.bin','cls-file-2.bin','cls-file-3.bin') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.xattr.tier'))='archive_me'" \
        2>/dev/null || echo 0)
    [[ "$_tagged" -ge 3 ]] && break
    sleep 1
done
[[ "$_tagged" -ge 3 ]] || die "timeout: only $_tagged/3 files have tier=archive_me in catalog"
note "  -> all 3 files tagged: tier=archive_me ✓"

# Double-check via API query with Tags predicate
API_FIND=$(curl -sf -X POST "$API/api/entries/query" \
    -H 'content-type: application/json' \
    -d '{"predicate":{"op":"tags","match_tags":{"tier":"archive_me"}},"limit":100}' \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); rows=d.get("entries",d) if isinstance(d,dict) else d; print(len(rows))' 2>/dev/null || echo "0")
note "  -> API query (Tags predicate) returned $API_FIND entries with tier=archive_me"
assert_ge "$API_FIND" "3" "API Tags predicate query returns >= 3 entries"

# ── 12. Create and run archive policy ─────────────────────────────────────────
note "creating hsm_archive policy (match_tags: tier=archive_me)"
ARCH_BODY=$(cat <<JSON
{
  "name": "e2e_archive_tagged",
  "kind": "hsm_archive",
  "match_tags": {"tier": "archive_me"},
  "trigger": "1h",
  "action": {
    "max_count": 500,
    "nb_threads": 4,
    "hsm": {"archive_id": $ARCHIVE_ID}
  },
  "enabled": true
}
JSON
)
ARCH_RESP=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' \
    -d "$ARCH_BODY")
ARCH_ID=$(echo "$ARCH_RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> archive policy id=$ARCH_ID"

# Dry-run first — verify no archive requests submitted
note "dry-run archive policy..."
curl -sf -X POST "$API/api/policies/$ARCH_ID/run" \
    -H 'content-type: application/json' -d '{"dry_run":true}' >/dev/null
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-file-$i.bin" 2>/dev/null)
    assert_contains "$s" "0x00000000" "dry-run left cls-file-$i.bin unarchived"
done
note "  -> dry-run verified: no HSM requests submitted"

# Live run
note "running archive policy (live)..."
curl -sf -X POST "$API/api/policies/$ARCH_ID/run" \
    -H 'content-type: application/json' -d '{}' >/dev/null
note "  -> archive policy triggered"

# ── 13. Wait for HSM archive ──────────────────────────────────────────────────
note "waiting for HSM archive (hsmd → terrasync → OST copies)..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        [[ "$(lfs hsm_state "$TEST_DIR/cls-file-$i.bin" 2>/dev/null)" == *"archived"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break
    sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-file-$i.bin" 2>/dev/null || echo "unknown")
    [[ "$s" == *"archived"* ]] || die "cls-file-$i.bin not archived within ${HSM_SETTLE}s: $s"
    note "  -> cls-file-$i.bin: $s"
done
note "  -> all 3 files archived by hsmd+terrasync ✓"

# Verify physical copies in archive root
ARCHIVED_FILES=$(find "$HSM_ARCHIVE_ROOT" -type f | wc -l)
assert_ge "$ARCHIVED_FILES" "3" "archive root has >= 3 files"
note "  -> $ARCHIVED_FILES file(s) in archive root $HSM_ARCHIVE_ROOT ✓"

# ── 14. Wait for catalog to reflect archived state ────────────────────────────
note "waiting for changelog CL_HSM events to update catalog hsm_state=archived..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" -e \
        "SELECT COUNT(*) FROM entries WHERE name IN ('cls-file-1.bin','cls-file-2.bin','cls-file-3.bin') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state'))='archived'" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break
    sleep 2
done
[[ "$_n" -ge 3 ]] || die "timeout: only $_n/3 files have hsm_state=archived in catalog"
note "  -> catalog sm_status.hsm_state=archived for all 3 files ✓"

# Verify xattr tags are still intact after classification + archive cycle
_still_tagged=$(mysql -u root -N -B "$DB" -e \
    "SELECT COUNT(*) FROM entries WHERE name IN ('cls-file-1.bin','cls-file-2.bin','cls-file-3.bin') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.xattr.tier'))='archive_me'" \
    2>/dev/null || echo 0)
assert_eq "$_still_tagged" "3" "xattr tags preserved after archive cycle"
note "  -> xattr tags preserved after HSM archive ✓"

# ── 15. Create and run release policy ─────────────────────────────────────────
note "creating hsm_release policy (match_tags: tier=archive_me)"
REL_BODY=$(cat <<JSON
{
  "name": "e2e_release_tagged",
  "kind": "hsm_release",
  "match_tags": {"tier": "archive_me"},
  "trigger": "1h",
  "action": {"max_count": 10, "nb_threads": 2},
  "enabled": true
}
JSON
)
REL_RESP=$(curl -sf -X POST "$API/api/policies" \
    -H 'content-type: application/json' \
    -d "$REL_BODY")
REL_ID=$(echo "$REL_RESP" | python3 -c 'import json,sys;print(json.load(sys.stdin)["id"])')
note "  -> release policy id=$REL_ID"

note "running release policy..."
curl -sf -X POST "$API/api/policies/$REL_ID/run" \
    -H 'content-type: application/json' -d '{}' >/dev/null
note "  -> release policy triggered"

# ── 16. Wait for release ──────────────────────────────────────────────────────
note "waiting for HSM release (OST data evicted)..."
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt $_deadline ]]; do
    _all=1
    for i in 1 2 3; do
        [[ "$(lfs hsm_state "$TEST_DIR/cls-file-$i.bin" 2>/dev/null)" == *"released"* ]] || { _all=0; break; }
    done
    [[ $_all -eq 1 ]] && break
    sleep 2
done
for i in 1 2 3; do
    s=$(lfs hsm_state "$TEST_DIR/cls-file-$i.bin" 2>/dev/null || echo "unknown")
    [[ "$s" == *"released"* ]] || die "cls-file-$i.bin not released within ${HSM_SETTLE}s: $s"
    note "  -> cls-file-$i.bin: $s"
done
note "  -> all 3 files released (data off OSTs) ✓"

# ── 17. Wait for catalog to reflect released state ────────────────────────────
note "waiting for changelog CL_HSM events to update catalog hsm_state=released..."
_deadline=$((SECONDS + SETTLE)); _n=0
while [[ $SECONDS -lt $_deadline ]]; do
    _n=$(mysql -u root -N -B "$DB" -e \
        "SELECT COUNT(*) FROM entries WHERE name IN ('cls-file-1.bin','cls-file-2.bin','cls-file-3.bin') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.hsm_state'))='released'" \
        2>/dev/null || echo 0)
    [[ "$_n" -ge 3 ]] && break
    sleep 2
done
[[ "$_n" -ge 3 ]] || die "timeout: only $_n/3 files have hsm_state=released in catalog"
note "  -> catalog sm_status.hsm_state=released for all 3 files ✓"

# ── 18. Restore one file and verify ──────────────────────────────────────────
note "restoring cls-file-1.bin via lfs hsm_restore"
lfs hsm_restore "$TEST_DIR/cls-file-1.bin" || die "lfs hsm_restore failed"
# Wait for "released" flag to DISAPPEAR — the file starts as "released exists archived"
# and should transition to "exists archived" once the restore is complete.
_deadline=$((SECONDS + HSM_SETTLE))
while [[ $SECONDS -lt _deadline ]]; do
    s=$(lfs hsm_state "$TEST_DIR/cls-file-1.bin" 2>/dev/null)
    [[ "$s" != *"released"* ]] && break
    sleep 2
done
s=$(lfs hsm_state "$TEST_DIR/cls-file-1.bin")
assert_contains "$s" "archived" "cls-file-1.bin restored: archived flag present"
[[ "$s" != *"released"* ]] || die "cls-file-1.bin still released after ${HSM_SETTLE}s restore, got: $s"
BYTES=$(wc -c < "$TEST_DIR/cls-file-1.bin")
assert_ge "$BYTES" "1" "cls-file-1.bin data readable after restore"
note "  -> cls-file-1.bin restored: $BYTES bytes readable, state: $s ✓"

# ── 19. Final consistency check ───────────────────────────────────────────────
note "final consistency check..."

# All 3 files must still have tier=archive_me tag
_final_tagged=$(mysql -u root -N -B "$DB" -e \
    "SELECT COUNT(*) FROM entries WHERE name IN ('cls-file-1.bin','cls-file-2.bin','cls-file-3.bin') AND JSON_UNQUOTE(JSON_EXTRACT(sm_status,'$.xattr.tier'))='archive_me'" \
    2>/dev/null || echo 0)
assert_eq "$_final_tagged" "3" "all 3 files still have tier=archive_me tag at end"
note "  -> xattr tags intact throughout pipeline ✓"

# Catalog count sanity
TOTAL=$(mysql -u root -N -B "$DB" -e "SELECT COUNT(*) FROM entries" 2>/dev/null || echo 0)
assert_ge "$TOTAL" "3" "catalog has >= 3 entries"
note "  -> catalog total entries: $TOTAL"

# Verify classifier is still listed
CLS_LIST=$(curl -sf "$API/api/classifiers")
assert_contains "$CLS_LIST" "e2e_tier_classifier" "classifier still listed after run"
note "  -> classifier still queryable via API ✓"

echo ""
echo "========================================"
echo "  ALL TESTS PASSED ✓"
echo "========================================"
echo ""
echo "Pipeline validated:"
echo "  Classifier tags  → sm_status.xattr.tier=archive_me"
echo "  Archive policy   → HSM archive (via hsmd+terrasync)"
echo "  Changelog events → sm_status.hsm_state=archived"
echo "  Release policy   → HSM release"
echo "  Changelog events → sm_status.hsm_state=released"
echo "  Restore          → data readable, state=archived"
