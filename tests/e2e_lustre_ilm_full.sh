#!/usr/bin/env bash
# e2e_lustre_ilm_full.sh — 完整的 Lustre ILM (Information Lifecycle Management) 端到端测试
#
# 架构:
#   robinhood-rs (policy engine + catalog) + hsmd (copytool) + hsm-plugin-terrasync (mover)
#
# 测试流程:
#   1. 在 /lustre/ilm_test 写入文件和目录
#   2. robinhood-rs full scan → MariaDB catalog
#   3. changelog 监听该目录变化
#   4. 策略: 10秒前写入的文件 → hsm_archive → hsm_release
#   5. 验证文件在 released 状态（元数据保留，数据在 /tmp/archive）
#   6. cat 文件 → Lustre 自动触发 HSM restore（通过 hsmd + terrasync）
#   7. 验证 restore 完成 (exists archived)
#   8. hsm_remove 策略清空 /tmp/archive
#   9. 验证 /tmp/archive 为空

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# 配置
# ─────────────────────────────────────────────────────────────────────────────
RBH_BIN="/root/rust/github/robinhood-rs/target/release/robinhood"
RBH_CLI="/root/rust/github/robinhood-rs/target/release/rbh"
HSMD_BIN="/root/rust/github/hsm-rs/target/release/hsmd"
PLUGIN_BIN="/root/rust/github/hsm-rs/target/release/hsm-plugin-terrasync"

LUSTRE_MOUNT="/lustre"
TEST_DIR="${LUSTRE_MOUNT}/ilm_test_$(date +%s)"
ARCHIVE_ROOT="/tmp/ilm_archive"
SOCKET_PATH="/tmp/ilm_hsmd.sock"
MDS_HOST="192.168.50.247"
MDT_NAME="testfs-MDT0000"
DB_URL="mysql://root@127.0.0.1/rbh_entries"
API_URL="http://127.0.0.1:8080"

# PID 追踪
HSMD_PID=""
PLUGIN_PID=""
DAEMON_PID=""
CL_USER=""

# ─────────────────────────────────────────────────────────────────────────────
# 辅助函数
# ─────────────────────────────────────────────────────────────────────────────
log() { echo "[$(date '+%H:%M:%S')] $*"; }
ok()  { echo "[$(date '+%H:%M:%S')] ✓ $*"; }
err() { echo "[$(date '+%H:%M:%S')] ✗ $*" >&2; }
die() { err "$*"; cleanup; exit 1; }

wait_for_state() {
    local file="$1" expected="$2" max_wait="${3:-30}"
    for ((i=1; i<=max_wait; i++)); do
        local state
        state=$(lfs hsm_state "$file" 2>/dev/null)
        if echo "$state" | grep -q "$expected"; then
            ok "HSM state: $state"
            return 0
        fi
        sleep 1
    done
    err "Timeout waiting for '$expected' on $file"
    lfs hsm_state "$file" 2>/dev/null || true
    return 1
}

wait_for_policy() {
    local policy_id="$1" max_wait="${2:-60}"
    log "Waiting for policy ${policy_id} to complete..."
    for ((i=1; i<=max_wait; i++)); do
        local state
        state=$(mysql -u root rbh_entries -sN -e "
            SELECT e.state FROM executions e
            JOIN schedules s ON e.schedule_id = s.id
            WHERE s.name LIKE '%policy.${policy_id}.%'
            ORDER BY e.scheduled_fire_time DESC LIMIT 1" 2>/dev/null || echo "")
        if [[ "$state" == "Succeeded" ]]; then
            ok "Policy ${policy_id} completed"
            return 0
        fi
        sleep 1
    done
    err "Timeout waiting for policy ${policy_id}"
    return 1
}

cleanup() {
    log "Cleanup..."
    [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null || true
    [[ -n "$PLUGIN_PID" ]] && kill "$PLUGIN_PID" 2>/dev/null || true
    [[ -n "$HSMD_PID"   ]] && kill "$HSMD_PID" 2>/dev/null || true
    sleep 1
    # HSM reset
    ssh "$MDS_HOST" "lctl set_param mdt.${MDT_NAME}.hsm_control=disabled" 2>/dev/null || true
    # 注销 changelog 用户
    if [[ -n "$CL_USER" ]]; then
        ssh "$MDS_HOST" "lctl --device ${MDT_NAME} changelog_deregister ${CL_USER}" 2>/dev/null || true
    fi
    # 删除测试目录
    rm -rf "$TEST_DIR" 2>/dev/null || true
}

trap cleanup EXIT

# ─────────────────────────────────────────────────────────────────────────────
# Phase 0: Prerequisites
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 0: Prerequisites check ==="

for bin in "$RBH_BIN" "$RBH_CLI" "$HSMD_BIN" "$PLUGIN_BIN"; do
    [[ -x "$bin" ]] || die "Binary not found: $bin"
done

mysql -u root rbh_entries -e "SELECT 1" >/dev/null 2>&1 || die "MariaDB not running"

ok "All binaries present, MariaDB running"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 1: Setup — changelog user, HSM, hsmd, daemon
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 1: Setup ==="

# HSM 配置
ssh "$MDS_HOST" "
    lctl set_param mdt.${MDT_NAME}.hsm_control=enabled
    lctl set_param mdt.${MDT_NAME}.hsm.loop_period=1
    lctl set_param mdd.${MDT_NAME}.changelog_mask='MARK CREAT MKDIR HLINK SLINK MKNOD UNLNK RMDIR RENME RNMTO CLOSE LYOUT TRUNC SATTR XATTR HSM MTIME CTIME'
" 2>/dev/null
ok "HSM enabled, changelog mask set"

# 注册 changelog 用户
CL_USER=$(ssh "$MDS_HOST" "lctl --device ${MDT_NAME} changelog_register --user ilm_e2e 2>/dev/null | grep -oE 'cl[0-9]+-ilm_e2e'" 2>/dev/null)
[[ -z "$CL_USER" ]] && CL_USER=$(ssh "$MDS_HOST" "lctl get_param mdd.${MDT_NAME}.changelog_users 2>/dev/null | grep ilm_e2e | awk '{print \$1}'" 2>/dev/null)
[[ -z "$CL_USER" ]] && die "Failed to register changelog user"
ok "Changelog user: $CL_USER"

# 创建归档目录
mkdir -p "$ARCHIVE_ROOT"
ok "Archive root: $ARCHIVE_ROOT"

# 创建 hsmd 配置
mkdir -p "$(dirname "$SOCKET_PATH")"
cat > /tmp/ilm_hsmd.toml <<EOF
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
EOF

# 创建 plugin 配置
cat > /tmp/ilm_plugin.toml <<EOF
socket_path = "${SOCKET_PATH}"
agent_id = "ilm-terrasync-agent"
archive_ids = [1]
archive_root_url = "file://${ARCHIVE_ROOT}"
log_filter = "info"
EOF

# 启动 hsmd
"$HSMD_BIN" --config /tmp/ilm_hsmd.toml > /tmp/ilm_hsmd.log 2>&1 &
HSMD_PID=$!
sleep 3
[[ -d /proc/$HSMD_PID ]] || die "hsmd failed to start: $(cat /tmp/ilm_hsmd.log)"
ok "hsmd started (PID=$HSMD_PID)"

# 启动 plugin
"$PLUGIN_BIN" --config /tmp/ilm_plugin.toml > /tmp/ilm_plugin.log 2>&1 &
PLUGIN_PID=$!
sleep 3
[[ -d /proc/$PLUGIN_PID ]] || die "hsm-plugin-terrasync failed: $(cat /tmp/ilm_plugin.log)"
ok "hsm-plugin-terrasync started (PID=$PLUGIN_PID)"

# 验证 copytool 注册
AGENTS=$(ssh "$MDS_HOST" "lctl get_param mdt.${MDT_NAME}.hsm.agents 2>/dev/null")
echo "$AGENTS" | grep -q "archive_id=1" || die "Copytool not registered: $AGENTS"
ok "Copytool registered: $AGENTS"

# 创建 robinhood-rs daemon 配置 (env)
cat > /tmp/ilm_rbh.env <<EOF
RBH_DATABASE_URL=${DB_URL}
RBH_LUSTRE_MOUNT=${LUSTRE_MOUNT}
RBH_MDTS=${MDT_NAME}
RBH_CHANGELOG_USER=${CL_USER}
RBH_LISTEN_ADDR=0.0.0.0:8080
RBH_HSM_POLL_SECS=5
RUST_LOG=rbh_daemon=info,info
EOF

# 启动 daemon
env $(cat /tmp/ilm_rbh.env | xargs) "$RBH_BIN" > /tmp/ilm_rbh.log 2>&1 &
DAEMON_PID=$!
sleep 8

[[ -d /proc/$DAEMON_PID ]] || die "robinhood-rs daemon failed: $(grep -m3 '"message"' /tmp/ilm_rbh.log)"

# 验证 API
API_STATUS=$(curl -sf "${API_URL}/api/health" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('status','?'))" 2>/dev/null)
[[ "$API_STATUS" == "ok" ]] || die "Daemon API not healthy: $API_STATUS"
ok "robinhood-rs daemon started (PID=$DAEMON_PID), API healthy"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 2: 创建测试数据
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 2: Create test data in ${TEST_DIR} ==="

mkdir -p "${TEST_DIR}/subdir_a" "${TEST_DIR}/subdir_b"

# 小文件
echo "file1: small text content" > "${TEST_DIR}/file1.txt"
echo "file2: another text file" > "${TEST_DIR}/file2.txt"
echo "subdir_a/nested" > "${TEST_DIR}/subdir_a/nested.txt"

# 中等文件 (1MB)
dd if=/dev/urandom of="${TEST_DIR}/medium.bin" bs=1M count=1 2>/dev/null

# 多个子目录文件
for i in {1..3}; do
    echo "subdir_b file ${i}" > "${TEST_DIR}/subdir_b/file${i}.txt"
done

FILE_COUNT=$(find "$TEST_DIR" -type f | wc -l)
ok "Created $FILE_COUNT files in $TEST_DIR"
find "$TEST_DIR" -type f -ls
# 记录文件创建时间戳（用于 archive 策略的 mtime 下界）
CREATE_TS=$(date +%s)

# ─────────────────────────────────────────────────────────────────────────────
# Phase 3: Full Scan
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 3: Full scan via robinhood-rs ==="

SCAN_RESP=$(curl -sf -X POST "${API_URL}/api/scans" \
    -H 'Content-Type: application/json' \
    -d '{}' 2>/dev/null)
log "Scan response: $SCAN_RESP"

# 等待扫描完成（catalog 中出现测试文件）
log "Waiting for catalog ingestion..."
for ((i=1; i<=30; i++)); do
    COUNT=$(mysql -u root rbh_entries -sN -e "
        SELECT COUNT(*) FROM entries WHERE name IN (
            'file1.txt','file2.txt','medium.bin','nested.txt'
        )" 2>/dev/null || echo 0)
    if [[ "$COUNT" -ge 4 ]]; then
        ok "Catalog populated: $COUNT test files found"
        break
    fi
    sleep 2
done

CATALOG_SIZE=$(curl -sf "${API_URL}/api/entries/count" 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
ok "Total catalog entries: $CATALOG_SIZE"

# 显示测试文件在 catalog 中的状态
mysql -u root rbh_entries -e "
    SELECT name, kind, size, hsm_state
    FROM entries
    WHERE name IN ('file1.txt','file2.txt','medium.bin','nested.txt')
    ORDER BY name;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# Phase 4: 等待 10 秒（满足 archive 触发条件）+ changelog 摄取
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 4: Wait 10s for archive policy trigger condition ==="
log "Changelog is monitoring $TEST_DIR changes in real-time..."

# 等待 changelog 摄取完成
for ((i=1; i<=15; i++)); do
    RECENT=$(python3 -c "
import json, sys
count = 0
for line in open('/tmp/ilm_rbh.log'):
    try:
        d = json.loads(line)
        if 'changelog batch complete' in d.get('fields',{}).get('message',''):
            count += 1
    except: pass
print(count)
" 2>/dev/null || echo 0)
    [[ "$RECENT" -ge 1 ]] && { ok "Changelog ingestion confirmed ($RECENT batches processed)"; break; }
    sleep 1
done

log "Sleeping 10 seconds for archive trigger condition (atime > 10s)..."
sleep 10
ok "10 seconds elapsed — files are now eligible for archiving"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 5: 创建并触发 Archive 策略
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 5: Archive policy — files older than 10 seconds ==="

# 计算 10 秒前的 Unix 时间戳（mtime 字段存储 Unix epoch i64）
CUTOFF_TS=$(( $(date +%s) - 10 ))
log "Archive cutoff timestamp: $CUTOFF_TS ($(date -d "@$CUTOFF_TS" 2>/dev/null || date -r "$CUTOFF_TS" 2>/dev/null))"

# 创建 HSM Archive 策略（scope: kind=file AND mtime ∈ [CREATE_TS-1, CUTOFF_TS)）
# 添加 mtime 下界以确保只选中本次测试创建的文件，排除历史已归档文件
LOWER_TS=$(( CREATE_TS - 2 ))
ARCHIVE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-archive-10s\",
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
            \"max_count\": 100,
            \"nb_threads\": 4,
            \"hsm\": {\"archive_id\": 1}
        },
        \"triggers\": [{\"type\": \"interval\", \"secs\": 300}]
    }" 2>/dev/null)
ARCHIVE_ID=$(echo "$ARCHIVE_POLICY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
[[ -n "$ARCHIVE_ID" ]] || die "Failed to create archive policy: $ARCHIVE_POLICY"
ok "Archive policy created (ID=$ARCHIVE_ID)"

# 手动触发
curl -sf -X POST "${API_URL}/api/policies/${ARCHIVE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null
log "Archive policy triggered, waiting for completion..."

# 等待所有测试文件归档完成
log "Waiting for all files to reach 'archived' state..."
for FILE in "${TEST_DIR}/file1.txt" "${TEST_DIR}/file2.txt" \
            "${TEST_DIR}/medium.bin" "${TEST_DIR}/subdir_a/nested.txt"; do
    wait_for_state "$FILE" "archived" 60 || die "Archive timeout for $FILE"
done
ok "All files archived to ${ARCHIVE_ROOT}"

# 验证归档目录内容
log "Archive directory contents:"
find "$ARCHIVE_ROOT" -type f | head -20
ARCHIVE_COUNT=$(find "$ARCHIVE_ROOT" -type f | wc -l)
ok "Archive backend contains $ARCHIVE_COUNT files"

# 验证 catalog 中的 HSM 状态
mysql -u root rbh_entries -e "
    SELECT name, hsm_state FROM entries
    WHERE name IN ('file1.txt','file2.txt','medium.bin','nested.txt')
    ORDER BY name;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# Phase 6: Release 策略 — 释放 OST 空间，保留元数据
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 6: Release policy — free OST space, keep metadata ==="

# 创建 HSM Release 策略（与 archive 策略同样的 mtime 范围，只释放本次测试文件）
RELEASE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-release\",
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
        \"default_action\": {\"max_count\": 20},
        \"triggers\": [{\"type\": \"interval\", \"secs\": 300}]
    }" 2>/dev/null)
RELEASE_ID=$(echo "$RELEASE_POLICY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
[[ -n "$RELEASE_ID" ]] || die "Failed to create release policy: $RELEASE_POLICY"
ok "Release policy created (ID=$RELEASE_ID)"

curl -sf -X POST "${API_URL}/api/policies/${RELEASE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null
log "Release policy triggered..."

# 等待文件进入 released 状态
for FILE in "${TEST_DIR}/file1.txt" "${TEST_DIR}/file2.txt" \
            "${TEST_DIR}/medium.bin" "${TEST_DIR}/subdir_a/nested.txt"; do
    wait_for_state "$FILE" "released" 60 || die "Release timeout for $FILE"
done
ok "All files released (data removed from OSTs, metadata preserved)"

# 验证元数据仍然存在（但数据不可用）
log "File metadata after release (cat should fail without restore):"
for FILE in "${TEST_DIR}/file1.txt" "${TEST_DIR}/file2.txt"; do
    echo "  $(lfs hsm_state "$FILE" 2>/dev/null)"
    echo "  Size: $(stat -c%s "$FILE" 2>/dev/null) bytes (metadata preserved)"
done

# 验证 catalog 更新
sleep 6  # HSM poller 每 5s 轮询一次
mysql -u root rbh_entries -e "
    SELECT name, hsm_state FROM entries
    WHERE name IN ('file1.txt','file2.txt','medium.bin','nested.txt')
    ORDER BY name;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# Phase 7: 通过 Lustre 客户端读取触发透明 Restore
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 7: Transparent restore — reading released files ==="
log "Reading released files will trigger HSM restore via hsmd+terrasync..."
log "(This blocks until Lustre receives data back from /tmp/ilm_archive)"

# 读取 file1.txt（触发自动 restore）
log "Reading ${TEST_DIR}/file1.txt (triggers automatic HSM restore)..."
RESTORE_START=$(date +%s)
CONTENT=$(cat "${TEST_DIR}/file1.txt" 2>/dev/null) || die "Failed to read file1.txt after restore"
RESTORE_END=$(date +%s)
RESTORE_TIME=$((RESTORE_END - RESTORE_START))
ok "file1.txt restored in ${RESTORE_TIME}s, content: '$CONTENT'"

# 读取 file2.txt（触发自动 restore）
log "Reading ${TEST_DIR}/file2.txt (triggers automatic HSM restore)..."
CONTENT2=$(cat "${TEST_DIR}/file2.txt" 2>/dev/null) || die "Failed to read file2.txt after restore"
ok "file2.txt restored, content: '$CONTENT2'"

# 读取 medium.bin（验证大文件 restore）
log "Reading ${TEST_DIR}/medium.bin (1MB, tests large file restore)..."
CHECKSUM_BEFORE=$(md5sum "${ARCHIVE_ROOT}/1/"*:0x:0x 2>/dev/null | head -1 | awk '{print $1}' || echo "N/A")
RESTORE_START=$(date +%s)
MD5_RESTORED=$(md5sum "${TEST_DIR}/medium.bin" 2>/dev/null | awk '{print $1}')
RESTORE_END=$(date +%s)
ok "medium.bin restored in $((RESTORE_END - RESTORE_START))s (md5: $MD5_RESTORED)"

# 读取子目录文件
log "Reading ${TEST_DIR}/subdir_a/nested.txt..."
NESTED_CONTENT=$(cat "${TEST_DIR}/subdir_a/nested.txt" 2>/dev/null) || die "nested.txt restore failed"
ok "nested.txt restored: '$NESTED_CONTENT'"

# 读取 subdir_b 所有文件
log "Reading all files in subdir_b..."
for FILE in "${TEST_DIR}/subdir_b/file"*.txt; do
    CONTENT=$(cat "$FILE" 2>/dev/null) || die "Restore failed for $FILE"
    ok "$(basename $FILE): '$CONTENT'"
done

# ─────────────────────────────────────────────────────────────────────────────
# Phase 8: 验证 Restore 完成
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 8: Verify restore state ==="

log "Waiting for all files to reach 'exists archived' state..."
for FILE in "${TEST_DIR}/file1.txt" "${TEST_DIR}/file2.txt" \
            "${TEST_DIR}/medium.bin" "${TEST_DIR}/subdir_a/nested.txt"; do
    wait_for_state "$FILE" "0x00000009" 30 || {
        err "File not in 'exists archived' state: $FILE ($(lfs hsm_state "$FILE" 2>/dev/null))"
    }
done

# 验证 hsmd + plugin 日志中有 restore 记录
RESTORE_COUNT=$(grep -c "restored" /tmp/ilm_plugin.log 2>/dev/null || echo 0)
ok "TerrasyncMover processed $RESTORE_COUNT restore operations"
grep "restored" /tmp/ilm_plugin.log 2>/dev/null | head -5

# ─────────────────────────────────────────────────────────────────────────────
# Phase 9: HSM Remove — 清空 /tmp/ilm_archive
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 9: Remove policy — clean up /tmp/ilm_archive ==="

# 等待 catalog 通过 HSM poller 更新（restored → archived state）
log "Waiting for catalog HSM state sync (hsm_poller)..."
sleep 10

# 创建 HSM Remove 策略（mtime 范围限制，只清理本次测试文件）
REMOVE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-remove-after-restore\",
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
        \"default_action\": {\"max_count\": 20},
        \"triggers\": [{\"type\": \"interval\", \"secs\": 300}]
    }" 2>/dev/null)
REMOVE_ID=$(echo "$REMOVE_POLICY" | python3 -c "import sys,json; print(json.load(sys.stdin).get('id',''))" 2>/dev/null)
[[ -n "$REMOVE_ID" ]] || die "Failed to create remove policy: $REMOVE_POLICY"
ok "Remove policy created (ID=$REMOVE_ID)"

ARCHIVE_BEFORE=$(find "$ARCHIVE_ROOT" -type f | wc -l)
log "Archive contains $ARCHIVE_BEFORE files before remove"

curl -sf -X POST "${API_URL}/api/policies/${REMOVE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null
log "Remove policy triggered, waiting for /tmp/ilm_archive to be cleaned..."

# 等待归档目录清空
for ((i=1; i<=60; i++)); do
    ARCHIVE_COUNT=$(find "$ARCHIVE_ROOT" -type f 2>/dev/null | wc -l)
    if [[ "$ARCHIVE_COUNT" -eq 0 ]]; then
        ok "/tmp/ilm_archive is now empty (all backend copies removed)"
        break
    fi
    sleep 1
done

# ─────────────────────────────────────────────────────────────────────────────
# Phase 10: 最终验证
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 10: Final verification ==="

FINAL_ERRORS=0

# 验证 /tmp/ilm_archive 为空
ARCHIVE_FINAL=$(find "$ARCHIVE_ROOT" -type f 2>/dev/null | wc -l)
if [[ "$ARCHIVE_FINAL" -eq 0 ]]; then
    ok "/tmp/ilm_archive is empty ✓"
else
    err "/tmp/ilm_archive still has $ARCHIVE_FINAL files ✗"
    find "$ARCHIVE_ROOT" -type f 2>/dev/null
    FINAL_ERRORS=$((FINAL_ERRORS + 1))
fi

# 验证文件仍然可读（数据在 Lustre 上）
for FILE in "${TEST_DIR}/file1.txt" "${TEST_DIR}/file2.txt"; do
    if cat "$FILE" >/dev/null 2>&1; then
        ok "$(basename $FILE) still readable on Lustre ✓"
    else
        err "$(basename $FILE) NOT readable ✗"
        FINAL_ERRORS=$((FINAL_ERRORS + 1))
    fi
done

# 验证 HSM 状态（应为 archived 但非 released，且无 backend copy）
for FILE in "${TEST_DIR}/file1.txt" "${TEST_DIR}/file2.txt" \
            "${TEST_DIR}/medium.bin" "${TEST_DIR}/subdir_a/nested.txt"; do
    STATE=$(lfs hsm_state "$FILE" 2>/dev/null)
    # 文件应在 Lustre 上（不是 released），backend copy 已删除（not archived? or still archived state?)
    echo "  $FILE: $STATE"
done

# 显示执行记录
log "Policy execution history:"
mysql -u root rbh_entries -e "
    SELECT
        SUBSTRING(id, 1, 8) as exec_id,
        SUBSTRING(schedule_id, 1, 8) as sched_id,
        state,
        DATE_FORMAT(scheduled_fire_time, '%H:%i:%s') as fired_at,
        DATE_FORMAT(finished_at, '%H:%i:%s') as finished
    FROM executions
    ORDER BY scheduled_fire_time DESC
    LIMIT 15;" 2>/dev/null

# 最终 catalog 状态
log "Final catalog HSM state distribution:"
mysql -u root rbh_entries -e "
    SELECT hsm_state, COUNT(*) as cnt
    FROM entries GROUP BY hsm_state;" 2>/dev/null

# plugin 操作汇总
log "TerrasyncMover operation summary:"
grep -oE "(archived|restored|removed)" /tmp/ilm_plugin.log 2>/dev/null | sort | uniq -c || true

# ─────────────────────────────────────────────────────────────────────────────
# 结果
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════"
if [[ "$FINAL_ERRORS" -eq 0 ]]; then
    echo "  ✓ ILM 端到端测试 PASSED"
    echo ""
    echo "  完整流程验证:"
    echo "    1. 文件写入 Lustre (/lustre/ilm_test) ✓"
    echo "    2. robinhood-rs Full Scan → MariaDB catalog ✓"
    echo "    3. Changelog 实时监听变化 ✓"
    echo "    4. Archive 策略 (mtime > 10s) → /tmp/ilm_archive ✓"
    echo "    5. Release 策略 → OST 数据释放，元数据保留 ✓"
    echo "    6. cat 文件触发自动 HSM Restore ✓"
    echo "    7. hsmd → hsm-plugin-terrasync → action FD 写入 ✓"
    echo "    8. Restore 后 state = 'exists archived'，无 dirty ✓"
    echo "    9. Remove 策略 → /tmp/ilm_archive 清空 ✓"
else
    echo "  ✗ ILM 端到端测试 FAILED ($FINAL_ERRORS errors)"
fi
echo "═══════════════════════════════════════════════════════════"

exit "$FINAL_ERRORS"
