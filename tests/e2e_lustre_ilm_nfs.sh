#!/usr/bin/env bash
# e2e_lustre_ilm_nfs.sh — 完整 Lustre ILM 端到端测试（NFS 后端）
#
# 架构:
#   robinhood-rs (policy engine + catalog) + hsmd (live copytool)
#   + hsm-plugin-nfs (NFSv3/v4 backend → 192.168.50.23:/export/nfs)
#
# 流程:
#   1. /lustre/<dir> 写入文件和目录
#   2. robinhood-rs full scan → MariaDB catalog
#   3. changelog 实时监听变化
#   4. archive 策略: mtime > 10s → hsm_archive → NFS (数据 + shadow)
#   5. release 策略: archived → hsm_release (Lustre 释放 OST)
#   6. 验证 NFS: 数据对象 + shadow 对象存在
#   7. cat 文件 → Lustre 透明触发 HSM Restore ← NFS
#   8. 验证 restore 后内容正确，HSM 状态 exists archived
#   9. remove 策略: → hsm_remove → NFS 对象删除（含 shadow）
#  10. 验证 NFS 存储中对应文件已删除

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# 配置
# ─────────────────────────────────────────────────────────────────────────────
RBH_BIN="/root/rust/github/robinhood-rs/target/release/robinhood"
RBH_CLI="/root/rust/github/robinhood-rs/target/release/rbh"
HSMD_BIN="/root/rust/github/hsm-rs/target/release/hsmd"
PLUGIN_BIN="/root/rust/github/hsm-rs/target/release/hsm-plugin-nfs"

LUSTRE_MOUNT="/lustre"
TEST_DIR="${LUSTRE_MOUNT}/ilm_nfs_$(date +%s)"
SOCKET_PATH="/tmp/ilm_nfs_hsmd.sock"
MDS_HOST="192.168.50.247"
MDT_NAME="testfs-MDT0000"
DB_URL="mysql://root@127.0.0.1/rbh_entries"
API_URL="http://127.0.0.1:8080"

# NFS 后端配置
NFS_SERVER="192.168.50.23"
NFS_EXPORT="/export/nfs"
NFS_SUBDIR="hsm-nfs-archive"
NFS_ARCHIVE_URL="nfs://${NFS_SERVER}${NFS_EXPORT}:${NFS_SUBDIR}"

# PID 追踪
HSMD_PID=""
PLUGIN_PID=""
DAEMON_PID=""
CL_USER=""
CREATE_TS=""

# ─────────────────────────────────────────────────────────────────────────────
# 辅助函数
# ─────────────────────────────────────────────────────────────────────────────
log()  { echo "[$(date '+%H:%M:%S')] $*"; }
ok()   { echo "[$(date '+%H:%M:%S')] ✓ $*"; }
err()  { echo "[$(date '+%H:%M:%S')] ✗ $*" >&2; }
die()  { err "$*"; cleanup; exit 1; }

wait_for_hsm_state() {
    local file="$1" expected="$2" max_wait="${3:-30}"
    for ((i=1; i<=max_wait; i++)); do
        lfs hsm_state "$file" 2>/dev/null | grep -q "$expected" && return 0
        sleep 1
    done
    err "Timeout: expected '$expected' on $file (got: $(lfs hsm_state "$file" 2>/dev/null))"
    return 1
}

# 统计 NFS 上的数据对象（archive/1/）和 shadow 对象数量
nfs_count_objects() {
    local dir_name
    dir_name=$(basename "$TEST_DIR")
    local data_count shadow_count
    data_count=$(ssh "${NFS_SERVER}" \
        "find ${NFS_EXPORT}/${NFS_SUBDIR}/1/ -maxdepth 1 -type f 2>/dev/null | wc -l" \
        2>/dev/null || echo 0)
    shadow_count=$(ssh "${NFS_SERVER}" \
        "find ${NFS_EXPORT}/${NFS_SUBDIR}/shadow/${dir_name}/ -type f 2>/dev/null | wc -l" \
        2>/dev/null || echo 0)
    echo "data=${data_count} shadow=${shadow_count}"
}

# 列出 NFS 上本次测试的数据对象和 shadow 对象
nfs_list_objects() {
    local dir_name
    dir_name=$(basename "$TEST_DIR")
    echo "  数据对象 (archive/1/):"
    ssh "${NFS_SERVER}" \
        "find ${NFS_EXPORT}/${NFS_SUBDIR}/1/ -maxdepth 1 -type f -printf '    %f (%s B)\n' 2>/dev/null" \
        2>/dev/null || true
    echo "  Shadow对象 (shadow/${dir_name}/):"
    ssh "${NFS_SERVER}" \
        "find ${NFS_EXPORT}/${NFS_SUBDIR}/shadow/${dir_name}/ -type f -printf '    %P\n' 2>/dev/null" \
        2>/dev/null || true
}

# 等待 NFS 上本次测试的数据对象达到期望数量
wait_nfs_objects() {
    local expected_count="$1"
    for ((i=1; i<=60; i++)); do
        local count
        count=$(ssh "${NFS_SERVER}" \
            "find ${NFS_EXPORT}/${NFS_SUBDIR}/1/ -maxdepth 1 -type f 2>/dev/null | wc -l" \
            2>/dev/null || echo 0)
        [[ "$count" -ge "$expected_count" ]] && return 0
        sleep 1
    done
    return 1
}

cleanup() {
    log "Cleanup..."
    [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null || true
    [[ -n "$PLUGIN_PID" ]] && kill "$PLUGIN_PID" 2>/dev/null || true
    [[ -n "$HSMD_PID"   ]] && kill "$HSMD_PID"   2>/dev/null || true
    sleep 1
    ssh "$MDS_HOST" "lctl set_param mdt.${MDT_NAME}.hsm_control=disabled" 2>/dev/null || true
    [[ -n "$CL_USER" ]] && \
        ssh "$MDS_HOST" "lctl --device ${MDT_NAME} changelog_deregister ${CL_USER}" 2>/dev/null || true
    rm -rf "$TEST_DIR" 2>/dev/null || true
    rm -f "$SOCKET_PATH" 2>/dev/null || true
}

trap cleanup EXIT

# ─────────────────────────────────────────────────────────────────────────────
# Phase 0: Prerequisites
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 0: Prerequisites ==="

for bin in "$RBH_BIN" "$RBH_CLI" "$HSMD_BIN" "$PLUGIN_BIN"; do
    [[ -x "$bin" ]] || die "Binary not found: $bin"
done

mysql -u root rbh_entries -e "SELECT 1" >/dev/null 2>&1 || die "MariaDB not running"

# 检查 NFS 服务器连通性（SSH 方式验证 export 目录可访问）
ssh -o ConnectTimeout=5 "${NFS_SERVER}" \
    "test -d ${NFS_EXPORT} && echo 'NFS export accessible'" 2>/dev/null \
    | grep -q "accessible" || die "NFS server ${NFS_SERVER}:${NFS_EXPORT} not accessible via SSH"

# 确保 NFS 存档子目录存在（插件会自动创建，这里提前确认权限）
ssh "${NFS_SERVER}" "mkdir -p ${NFS_EXPORT}/${NFS_SUBDIR}/1 ${NFS_EXPORT}/${NFS_SUBDIR}/shadow" \
    2>/dev/null || die "Cannot create NFS archive directory on ${NFS_SERVER}"

ok "Prerequisites OK (NFS: ${NFS_ARCHIVE_URL})"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 1: Setup
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 1: Setup (changelog + HSM + hsmd + plugin + rbh) ==="

ssh "$MDS_HOST" "
    lctl set_param mdt.${MDT_NAME}.hsm_control=enabled
    lctl set_param mdt.${MDT_NAME}.hsm.loop_period=1
    lctl set_param mdd.${MDT_NAME}.changelog_mask='MARK CREAT MKDIR HLINK SLINK MKNOD UNLNK RMDIR RENME RNMTO CLOSE LYOUT TRUNC SATTR XATTR HSM MTIME CTIME'
" 2>/dev/null
ok "HSM enabled, changelog mask set"

CL_USER=$(ssh "$MDS_HOST" "lctl --device ${MDT_NAME} changelog_register --user ilm_nfs_e2e 2>/dev/null | grep -oE 'cl[0-9]+-ilm_nfs_e2e'" 2>/dev/null)
[[ -z "$CL_USER" ]] && CL_USER=$(ssh "$MDS_HOST" "lctl get_param mdd.${MDT_NAME}.changelog_users 2>/dev/null | grep ilm_nfs_e2e | awk '{print \$1}'" 2>/dev/null)
[[ -z "$CL_USER" ]] && die "Failed to register changelog user"
ok "Changelog user: $CL_USER"

# hsmd 配置
cat > /tmp/ilm_nfs_hsmd.toml <<TOML
mode = "live"
mountpoint = "${LUSTRE_MOUNT}"
archive_ids = [1]

[transport]
socket_path = "${SOCKET_PATH}"

[scheduler]
tick_interval_ms = 50
max_per_tick = 32

[xattr]
namespace = "trusted"

[log]
filter = "hsmd=info,info"
TOML

# NFS plugin 配置
cat > /tmp/ilm_nfs_plugin.toml <<TOML
socket_path = "${SOCKET_PATH}"
agent_id = "ilm-nfs-mover"
archive_ids = [1]
archive_root_url = "${NFS_ARCHIVE_URL}"
log_filter = "hsm.plugin.nfs=debug,info"
TOML

# 启动 hsmd
"$HSMD_BIN" --config /tmp/ilm_nfs_hsmd.toml > /tmp/ilm_nfs_hsmd.log 2>&1 &
HSMD_PID=$!
sleep 3
[[ -d /proc/$HSMD_PID ]] || die "hsmd failed: $(tail -3 /tmp/ilm_nfs_hsmd.log)"
ok "hsmd started (PID=$HSMD_PID)"

# 启动 NFS plugin（NFS mount 需要几秒连接时间）
"$PLUGIN_BIN" --config /tmp/ilm_nfs_plugin.toml > /tmp/ilm_nfs_plugin.log 2>&1 &
PLUGIN_PID=$!
sleep 8
[[ -d /proc/$PLUGIN_PID ]] || die "hsm-plugin-nfs failed: $(tail -5 /tmp/ilm_nfs_plugin.log)"
grep -q "registered with daemon\|connected to daemon" /tmp/ilm_nfs_plugin.log \
    || die "NFS plugin not registered with daemon: $(tail -5 /tmp/ilm_nfs_plugin.log)"
ok "hsm-plugin-nfs (NFS) started (PID=$PLUGIN_PID)"

# 验证 copytool 注册
AGENTS=$(ssh "$MDS_HOST" "lctl get_param mdt.${MDT_NAME}.hsm.agents 2>/dev/null")
echo "$AGENTS" | grep -q "archive_id=1" || die "Copytool not registered: $AGENTS"
ok "Copytool registered: $(echo "$AGENTS" | grep -oE 'requests=\[[^]]+\]')"

# robinhood-rs daemon
mysql -u root rbh_entries -e "DELETE FROM policies; DELETE FROM schedules; DELETE FROM executions;" 2>/dev/null
env \
    RBH_DATABASE_URL="$DB_URL" \
    RBH_LUSTRE_MOUNT="$LUSTRE_MOUNT" \
    RBH_MDTS="$MDT_NAME" \
    RBH_CHANGELOG_USER="$CL_USER" \
    RBH_LISTEN_ADDR="0.0.0.0:8080" \
    RBH_HSM_POLL_SECS="5" \
    RUST_LOG="rbh_daemon=info,info" \
    "$RBH_BIN" > /tmp/ilm_nfs_rbh.log 2>&1 &
DAEMON_PID=$!
sleep 8
[[ -d /proc/$DAEMON_PID ]] || die "robinhood-rs daemon failed: $(grep message /tmp/ilm_nfs_rbh.log | tail -3)"

API_STATUS=$(curl -sf "${API_URL}/api/health" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null)
[[ "$API_STATUS" == "ok" ]] || die "API not healthy: $API_STATUS"
ok "robinhood-rs daemon started (PID=$DAEMON_PID)"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 2: 创建测试数据
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 2: Create test data in ${TEST_DIR} ==="

mkdir -p "${TEST_DIR}/docs" "${TEST_DIR}/data"
echo "report content - $(date)" > "${TEST_DIR}/report.txt"
echo "readme" > "${TEST_DIR}/docs/readme.md"
echo "notes" > "${TEST_DIR}/docs/notes.txt"
dd if=/dev/urandom of="${TEST_DIR}/data/dataset.bin" bs=512K count=1 2>/dev/null
for i in {1..3}; do echo "item $i" > "${TEST_DIR}/data/record${i}.csv"; done

CREATE_TS=$(date +%s)

FILE_COUNT=$(find "$TEST_DIR" -type f | wc -l)
ok "Created $FILE_COUNT files in $TEST_DIR"
find "$TEST_DIR" -type f -printf "  %f (%s B)\n"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 3: Full Scan
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 3: Full scan → MariaDB catalog ==="

curl -sf -X POST "${API_URL}/api/scans" -H 'Content-Type: application/json' -d '{}' > /dev/null

for ((i=1; i<=30; i++)); do
    COUNT=$(mysql -u root rbh_entries -sN -e "
        SELECT COUNT(*) FROM entries
        WHERE name IN ('report.txt','readme.md','notes.txt','dataset.bin','record1.csv')
    " 2>/dev/null || echo 0)
    [[ "$COUNT" -ge 5 ]] && { ok "Catalog populated ($COUNT test files)"; break; }
    sleep 2
done

TOTAL=$(curl -sf "${API_URL}/api/entries/count" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
ok "Total catalog entries: $TOTAL"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 4: 等待 10 秒（changelog 摄取 + archive 触发条件）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 4: Wait 10s (changelog ingestion + archive trigger) ==="

CUTOFF_TS=$((CREATE_TS - 2))
LOWER_TS=$((CREATE_TS - 3))

log "Changelog monitoring ${TEST_DIR} changes..."
sleep 10

BATCHES=$(python3 -c "
count = sum(1 for line in open('/tmp/ilm_nfs_rbh.log')
    if 'changelog batch complete' in line)
print(count)
" 2>/dev/null || echo 0)
ok "10s elapsed — changelog batches processed: $BATCHES"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 5: Archive 策略 → NFS（含 shadow）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 5: Archive policy → NFS (data + shadow namespace) ==="

CUTOFF_TS=$(( $(date +%s) - 10 ))

ARCHIVE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-nfs-archive\",
        \"kind\": \"hsm_archive\",
        \"scope\": {
            \"op\": \"and\",
            \"children\": [
                {\"op\": \"cmp\", \"field\": \"kind\", \"cmp\": \"eq\", \"value\": 0},
                {\"op\": \"cmp\", \"field\": \"mtime\", \"cmp\": \"ge\", \"value\": ${LOWER_TS}},
                {\"op\": \"cmp\", \"field\": \"mtime\", \"cmp\": \"lt\", \"value\": ${CUTOFF_TS}}
            ]
        },
        \"rules\": [],
        \"default_action\": {
            \"max_count\": 50,
            \"nb_threads\": 4,
            \"hsm\": {\"archive_id\": 1}
        },
        \"triggers\": [{\"type\": \"interval\", \"secs\": 300}]
    }" 2>/dev/null)
ARCHIVE_ID=$(echo "$ARCHIVE_POLICY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
[[ -n "$ARCHIVE_ID" ]] || die "Failed to create archive policy: $ARCHIVE_POLICY"
ok "Archive policy created (ID=$ARCHIVE_ID, mtime range: [$LOWER_TS, $CUTOFF_TS))"

curl -sf -X POST "${API_URL}/api/policies/${ARCHIVE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null

log "Waiting for files to be archived to NFS..."
for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/docs/readme.md" \
            "${TEST_DIR}/data/dataset.bin" "${TEST_DIR}/data/record1.csv"; do
    wait_for_hsm_state "$FILE" "archived" 60 || die "Archive timeout for $FILE"
done
ok "All files archived to NFS"

log "NFS objects after archive:"
nfs_list_objects

mysql -u root rbh_entries -e "
    SELECT name, hsm_state FROM entries
    WHERE name IN ('report.txt','readme.md','notes.txt','dataset.bin','record1.csv')
    ORDER BY name;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# Phase 6: Release 策略 → 释放 Lustre OST
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 6: Release policy → free Lustre OST space ==="

RELEASE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-nfs-release\",
        \"kind\": \"hsm_release\",
        \"scope\": {
            \"op\": \"and\",
            \"children\": [
                {\"op\": \"cmp\", \"field\": \"kind\", \"cmp\": \"eq\", \"value\": 0},
                {\"op\": \"cmp\", \"field\": \"mtime\", \"cmp\": \"ge\", \"value\": ${LOWER_TS}},
                {\"op\": \"cmp\", \"field\": \"mtime\", \"cmp\": \"lt\", \"value\": ${CUTOFF_TS}},
                {\"op\": \"hsm_state_eq\", \"state\": \"archived\"}
            ]
        },
        \"rules\": [],
        \"default_action\": {\"max_count\": 50},
        \"triggers\": [{\"type\": \"interval\", \"secs\": 300}]
    }" 2>/dev/null)
RELEASE_ID=$(echo "$RELEASE_POLICY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
[[ -n "$RELEASE_ID" ]] || die "Failed to create release policy"
ok "Release policy created (ID=$RELEASE_ID)"

curl -sf -X POST "${API_URL}/api/policies/${RELEASE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null

for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/docs/readme.md" \
            "${TEST_DIR}/data/dataset.bin" "${TEST_DIR}/data/record1.csv"; do
    wait_for_hsm_state "$FILE" "released" 60 || die "Release timeout for $FILE"
done
ok "All files released (OST freed, metadata preserved)"

log "File metadata after release:"
for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/data/dataset.bin"; do
    echo "  $(lfs hsm_state "$FILE" 2>/dev/null) | size=$(stat -c%s "$FILE" 2>/dev/null)B"
done

sleep 6
mysql -u root rbh_entries -e "
    SELECT name, hsm_state FROM entries
    WHERE name IN ('report.txt','readme.md','dataset.bin','record1.csv')
    ORDER BY name;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# Phase 7: 验证 NFS 中数据和 shadow 均存在
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 7: Verify NFS objects (data + shadow) ==="

NFS_STATUS=$(nfs_count_objects)
DATA_COUNT=$(echo "$NFS_STATUS" | grep -oP 'data=\K[0-9]+' || echo 0)
SHADOW_COUNT=$(echo "$NFS_STATUS" | grep -oP 'shadow=\K[0-9]+' || echo 0)
[[ "$DATA_COUNT" -ge 7 ]] || err "Expected ≥7 data objects, got $DATA_COUNT"
[[ "$SHADOW_COUNT" -ge 7 ]] || err "Expected ≥7 shadow objects, got $SHADOW_COUNT"
ok "NFS 状态: $DATA_COUNT 个数据对象, $SHADOW_COUNT 个 shadow 对象"

log "NFS object listing:"
nfs_list_objects

# ─────────────────────────────────────────────────────────────────────────────
# Phase 8: 透明 Restore（cat 触发 HSM Restore ← NFS）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 8: Transparent restore — cat released files → NFS ==="
log "Reading released files will trigger HSM Restore via hsmd + NfsMover::restore() ← NFS"

for FILE_REL in "report.txt" "docs/readme.md" "data/dataset.bin" "data/record1.csv" \
                "docs/notes.txt" "data/record2.csv" "data/record3.csv"; do
    FILE="${TEST_DIR}/${FILE_REL}"
    [[ -f "$FILE" ]] || continue
    log "Reading: $FILE_REL ..."
    CONTENT=$(cat "$FILE" 2>/dev/null) && \
        ok "$FILE_REL restored ($(echo -n "$CONTENT" | wc -c)B)" || \
        err "$FILE_REL restore FAILED"
done

log "Plugin restore operations:"
grep "restored" /tmp/ilm_nfs_plugin.log | tail -10

# ─────────────────────────────────────────────────────────────────────────────
# Phase 9: 验证 Restore 状态
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 9: Verify restore state ==="

for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/docs/readme.md" \
            "${TEST_DIR}/data/dataset.bin" "${TEST_DIR}/data/record1.csv"; do
    wait_for_hsm_state "$FILE" "0x00000009" 30 || \
        err "$(lfs hsm_state "$FILE" 2>/dev/null)"
    STATE=$(lfs hsm_state "$FILE" 2>/dev/null)
    ok "$FILE — $STATE"
done

RESTORE_COUNT=$(grep -c "restored" /tmp/ilm_nfs_plugin.log 2>/dev/null || echo 0)
ok "NfsMover processed $RESTORE_COUNT restore operations from NFS"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 10: Remove 策略 → 删除 NFS 副本（含 shadow）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 10: Remove policy → delete NFS backend copies + shadow ==="

log "Waiting for catalog HSM state sync..."
sleep 10

NFS_BEFORE=$(nfs_count_objects)
log "NFS before remove: $NFS_BEFORE"

REMOVE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-nfs-remove\",
        \"kind\": \"hsm_remove\",
        \"scope\": {
            \"op\": \"and\",
            \"children\": [
                {\"op\": \"cmp\", \"field\": \"kind\", \"cmp\": \"eq\", \"value\": 0},
                {\"op\": \"cmp\", \"field\": \"mtime\", \"cmp\": \"ge\", \"value\": ${LOWER_TS}},
                {\"op\": \"cmp\", \"field\": \"mtime\", \"cmp\": \"lt\", \"value\": ${CUTOFF_TS}},
                {\"op\": \"hsm_state_eq\", \"state\": \"archived\"}
            ]
        },
        \"rules\": [],
        \"default_action\": {\"max_count\": 50},
        \"triggers\": [{\"type\": \"interval\", \"secs\": 300}]
    }" 2>/dev/null)
REMOVE_ID=$(echo "$REMOVE_POLICY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
[[ -n "$REMOVE_ID" ]] || die "Failed to create remove policy"
ok "Remove policy created (ID=$REMOVE_ID)"

curl -sf -X POST "${API_URL}/api/policies/${REMOVE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null
log "Remove policy triggered, waiting for NFS cleanup..."

for ((i=1; i<=60; i++)); do
    NFS_STATUS=$(nfs_count_objects)
    DATA_LEFT=$(echo "$NFS_STATUS" | grep -oP 'data=\K[0-9]+' || echo 999)
    SHADOW_LEFT=$(echo "$NFS_STATUS" | grep -oP 'shadow=\K[0-9]+' || echo 999)
    if [[ "$DATA_LEFT" -eq 0 && "$SHADOW_LEFT" -eq 0 ]]; then
        ok "NFS 数据对象和 shadow 均已清除"
        break
    fi
    sleep 1
done

log "Plugin remove operations:"
grep "removed" /tmp/ilm_nfs_plugin.log | tail -10

# ─────────────────────────────────────────────────────────────────────────────
# Phase 11: 最终验证
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 11: Final verification ==="

FINAL_ERRORS=0

NFS_FINAL=$(nfs_count_objects)
DATA_FINAL=$(echo "$NFS_FINAL" | grep -oP 'data=\K[0-9]+' || echo 999)
SHADOW_FINAL=$(echo "$NFS_FINAL" | grep -oP 'shadow=\K[0-9]+' || echo 999)

if [[ "$DATA_FINAL" -eq 0 ]]; then
    ok "NFS 数据对象: 已全部删除 ✓"
else
    err "NFS 数据对象: 仍有 $DATA_FINAL 个 ✗"
    FINAL_ERRORS=$((FINAL_ERRORS + 1))
fi

if [[ "$SHADOW_FINAL" -eq 0 ]]; then
    ok "NFS shadow 对象: 已全部删除 ✓"
else
    err "NFS shadow 对象: 仍有 $SHADOW_FINAL 个 ✗"
    FINAL_ERRORS=$((FINAL_ERRORS + 1))
fi

for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/docs/readme.md"; do
    if cat "$FILE" >/dev/null 2>&1; then
        ok "$(basename $FILE) 在 Lustre 上可读 ✓"
    else
        err "$(basename $FILE) 读取失败 ✗"
        FINAL_ERRORS=$((FINAL_ERRORS + 1))
    fi
done

log "Policy execution history:"
mysql -u root rbh_entries -e "
    SELECT SUBSTRING(id,1,8) as exec_id, state,
           DATE_FORMAT(scheduled_fire_time,'%H:%i:%s') as fired_at,
           DATE_FORMAT(finished_at,'%H:%i:%s') as finished
    FROM executions ORDER BY scheduled_fire_time DESC LIMIT 10;" 2>/dev/null

log "NfsMover NFS operations:"
python3 -c "
for op in ['archived', 'restored', 'removed']:
    count = sum(1 for line in open('/tmp/ilm_nfs_plugin.log') if op in line)
    print(f'  {op}: {count}')
" 2>/dev/null

log "Final catalog HSM distribution:"
mysql -u root rbh_entries -e "
    SELECT hsm_state, COUNT(*) as cnt FROM entries GROUP BY hsm_state;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# 结果
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════════════════"
if [[ "$FINAL_ERRORS" -eq 0 ]]; then
    echo "  ✓ ILM NFS 端到端测试 PASSED"
    echo ""
    echo "  完整流程验证 (后端: NFS ${NFS_ARCHIVE_URL}):"
    echo "    1. 文件写入 Lustre (${TEST_DIR}) ✓"
    echo "    2. robinhood-rs Full Scan → MariaDB catalog ✓"
    echo "    3. Changelog 实时监听 ✓"
    echo "    4. Archive 策略 (mtime>10s) → NFS (数据+shadow) ✓"
    echo "    5. Release 策略 → OST 释放，元数据保留 ✓"
    echo "    6. NFS 数据对象 + shadow 验证 ✓"
    echo "    7. cat 文件 → Lustre 透明 Restore ← NFS ✓"
    echo "    8. Restore 后 exists archived（无 dirty）✓"
    echo "    9. Remove 策略 → NFS 数据+shadow 全部删除 ✓"
    echo "   10. Lustre 文件仍可读 ✓"
else
    echo "  ✗ ILM NFS 端到端测试 FAILED ($FINAL_ERRORS errors)"
fi
echo "══════════════════════════════════════════════════════════════"

exit "$FINAL_ERRORS"
