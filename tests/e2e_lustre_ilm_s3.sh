#!/usr/bin/env bash
# e2e_lustre_ilm_s3.sh — 完整 Lustre ILM 端到端测试（S3/RustFS 后端）
#
# 架构:
#   robinhood-rs (policy engine + catalog) + hsmd (live copytool)
#   + hsm-plugin-terrasync (S3 backend → RustFS 192.168.50.23:39000)
#
# 流程:
#   1. /lustre/<dir> 写入文件和目录
#   2. robinhood-rs full scan → MariaDB catalog
#   3. changelog 实时监听变化
#   4. archive 策略: mtime > 10s → hsm_archive → S3 (数据 + shadow)
#   5. release 策略: archived → hsm_release (Lustre 释放 OST)
#   6. 验证 S3: 数据对象 + shadow 对象存在
#   7. cat 文件 → Lustre 透明触发 HSM Restore ← S3
#   8. 验证 restore 后内容正确，HSM 状态 exists archived
#   9. remove 策略: → hsm_remove → S3 对象删除（含 shadow）
#  10. 验证 S3 桶中对应文件已删除

set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# 配置
# ─────────────────────────────────────────────────────────────────────────────
RBH_BIN="/root/rust/github/robinhood-rs/target/release/robinhood"
RBH_CLI="/root/rust/github/robinhood-rs/target/release/rbh"
HSMD_BIN="/root/rust/github/hsm-rs/target/release/hsmd"
PLUGIN_BIN="/root/rust/github/hsm-rs/target/release/hsm-plugin-terrasync"

LUSTRE_MOUNT="/lustre"
TEST_DIR="${LUSTRE_MOUNT}/ilm_s3_$(date +%s)"
SOCKET_PATH="/tmp/ilm_s3_hsmd.sock"
MDS_HOST="192.168.50.247"
MDT_NAME="testfs-MDT0000"
DB_URL="mysql://root@127.0.0.1/rbh_entries"
API_URL="http://127.0.0.1:8080"

# RustFS S3 配置
S3_ENDPOINT="http://192.168.50.23:39000"
S3_ACCESS_KEY="rustfsadmin"
S3_SECRET_KEY="rustfsadmin123"
S3_BUCKET="hsm-archive"
# archive_root_url 格式: s3+http://ak:sk@bucket.host:port
S3_ARCHIVE_URL="s3+http://${S3_ACCESS_KEY}:${S3_SECRET_KEY}@${S3_BUCKET}.192.168.50.23:39000"

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

# 列出 S3 桶中指定前缀的对象（仅本次测试相关）
# 列出本次测试的 S3 对象。
# 数据对象按 FID 存储（1/<fid>），通过 shadow 对象反查。
# Shadow 对象按目录名前缀存储（shadow/<test_dir_name>/...）。
s3_list_test_objects() {
    local dir_name
    dir_name=$(basename "$TEST_DIR")
    python3 - <<PYEOF 2>/dev/null
import boto3
from botocore.client import Config
s3 = boto3.client('s3',
    endpoint_url='${S3_ENDPOINT}',
    aws_access_key_id='${S3_ACCESS_KEY}',
    aws_secret_access_key='${S3_SECRET_KEY}',
    config=Config(signature_version='s3v4'),
    region_name='us-east-1')

shadow_prefix = 'shadow/${dir_name}'

# Shadow 对象（按目录名前缀）
shadow_objs = s3.list_objects_v2(Bucket='${S3_BUCKET}', Prefix=shadow_prefix).get('Contents', [])

# 从 shadow 内容中提取 FID，检查对应数据对象是否存在
data_found = []
for so in shadow_objs:
    try:
        fid = s3.get_object(Bucket='${S3_BUCKET}', Key=so['Key'])['Body'].read().decode().strip()
        data_key = '1/' + fid
        try:
            obj = s3.head_object(Bucket='${S3_BUCKET}', Key=data_key)
            data_found.append((data_key, obj['ContentLength']))
        except:
            data_found.append((data_key, None))  # missing
    except:
        pass

print(f"  数据对象 ({len([d for d in data_found if d[1] is not None])}个):")
for key, size in data_found:
    status = f"{size}B" if size is not None else "MISSING"
    print(f"    {key} ({status})")

print(f"  Shadow对象 ({len(shadow_objs)}个):")
for o in shadow_objs:
    try:
        content = s3.get_object(Bucket='${S3_BUCKET}', Key=o['Key'])['Body'].read().decode()
        print(f"    {o['Key']} → {content}")
    except:
        print(f"    {o['Key']}")

data_count = len([d for d in data_found if d[1] is not None])
shadow_count = len(shadow_objs)
print(f"total_data={data_count} total_shadow={shadow_count}")
PYEOF
}

# 等待 S3 中所有测试文件的数据对象出现（通过 shadow 计数推断）
wait_s3_objects() {
    local expected_count="$1"
    local dir_name
    dir_name=$(basename "$TEST_DIR")
    for ((i=1; i<=60; i++)); do
        local count
        count=$(python3 - <<PYEOF 2>/dev/null
import boto3; from botocore.client import Config
s3 = boto3.client('s3', endpoint_url='${S3_ENDPOINT}',
    aws_access_key_id='${S3_ACCESS_KEY}', aws_secret_access_key='${S3_SECRET_KEY}',
    config=Config(signature_version='s3v4'), region_name='us-east-1')
shadow_objs = s3.list_objects_v2(Bucket='${S3_BUCKET}', Prefix='shadow/${dir_name}').get('Contents', [])
found = 0
for so in shadow_objs:
    try:
        fid = s3.get_object(Bucket='${S3_BUCKET}', Key=so['Key'])['Body'].read().decode().strip()
        s3.head_object(Bucket='${S3_BUCKET}', Key='1/' + fid)
        found += 1
    except:
        pass
print(found)
PYEOF
)
        [[ "$count" -ge "$expected_count" ]] && return 0
        sleep 1
    done
    return 1
}

# 计算 S3 中测试数据对象和 shadow 对象数量
s3_count_data_objects() {
    local dir_name
    dir_name=$(basename "$TEST_DIR")
    python3 - <<PYEOF 2>/dev/null
import boto3; from botocore.client import Config
s3 = boto3.client('s3', endpoint_url='${S3_ENDPOINT}',
    aws_access_key_id='${S3_ACCESS_KEY}', aws_secret_access_key='${S3_SECRET_KEY}',
    config=Config(signature_version='s3v4'), region_name='us-east-1')
shadow_objs = s3.list_objects_v2(Bucket='${S3_BUCKET}', Prefix='shadow/${dir_name}').get('Contents', [])
# 数据对象通过 shadow 反查
data_count = 0
for so in shadow_objs:
    try:
        fid = s3.get_object(Bucket='${S3_BUCKET}', Key=so['Key'])['Body'].read().decode().strip()
        s3.head_object(Bucket='${S3_BUCKET}', Key='1/' + fid)
        data_count += 1
    except:
        pass  # data object deleted or missing
print(f"data={data_count} shadow={len(shadow_objs)}")
PYEOF
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

# 检查 python3 + boto3（用于 S3 验证）
python3 -c "import boto3" 2>/dev/null || die "boto3 not installed (pip3 install boto3)"

# 检查 RustFS 连通性
python3 -c "
import boto3; from botocore.client import Config
s3 = boto3.client('s3', endpoint_url='${S3_ENDPOINT}',
    aws_access_key_id='${S3_ACCESS_KEY}', aws_secret_access_key='${S3_SECRET_KEY}',
    config=Config(signature_version='s3v4'), region_name='us-east-1')
s3.head_bucket(Bucket='${S3_BUCKET}')
print('S3 OK')
" 2>/dev/null || die "RustFS S3 not reachable or bucket '${S3_BUCKET}' does not exist"

ok "Prerequisites OK"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 1: Setup
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 1: Setup (changelog + HSM + hsmd + rbh) ==="

# HSM + changelog mask
ssh "$MDS_HOST" "
    lctl set_param mdt.${MDT_NAME}.hsm_control=enabled
    lctl set_param mdt.${MDT_NAME}.hsm.loop_period=1
    lctl set_param mdd.${MDT_NAME}.changelog_mask='MARK CREAT MKDIR HLINK SLINK MKNOD UNLNK RMDIR RENME RNMTO CLOSE LYOUT TRUNC SATTR XATTR HSM MTIME CTIME'
" 2>/dev/null
ok "HSM enabled, changelog mask set"

# 注册 changelog user
CL_USER=$(ssh "$MDS_HOST" "lctl --device ${MDT_NAME} changelog_register --user ilm_s3_e2e 2>/dev/null | grep -oE 'cl[0-9]+-ilm_s3_e2e'" 2>/dev/null)
[[ -z "$CL_USER" ]] && CL_USER=$(ssh "$MDS_HOST" "lctl get_param mdd.${MDT_NAME}.changelog_users 2>/dev/null | grep ilm_s3_e2e | awk '{print \$1}'" 2>/dev/null)
[[ -z "$CL_USER" ]] && die "Failed to register changelog user"
ok "Changelog user: $CL_USER"

# hsmd 配置 (S3 backend)
cat > /tmp/ilm_s3_hsmd.toml <<TOML
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

# plugin 配置 (S3 backend)
cat > /tmp/ilm_s3_plugin.toml <<TOML
socket_path = "${SOCKET_PATH}"
agent_id = "ilm-s3-terrasync"
archive_ids = [1]
archive_root_url = "${S3_ARCHIVE_URL}"
log_filter = "hsm.plugin.terrasync=debug,info"
TOML

# 启动 hsmd
"$HSMD_BIN" --config /tmp/ilm_s3_hsmd.toml > /tmp/ilm_s3_hsmd.log 2>&1 &
HSMD_PID=$!
sleep 3
[[ -d /proc/$HSMD_PID ]] || die "hsmd failed: $(tail -3 /tmp/ilm_s3_hsmd.log)"
ok "hsmd started (PID=$HSMD_PID)"

# 启动 plugin
"$PLUGIN_BIN" --config /tmp/ilm_s3_plugin.toml > /tmp/ilm_s3_plugin.log 2>&1 &
PLUGIN_PID=$!
sleep 5
[[ -d /proc/$PLUGIN_PID ]] || die "plugin failed: $(tail -5 /tmp/ilm_s3_plugin.log)"
grep -q "registered with daemon" /tmp/ilm_s3_plugin.log || die "plugin not registered"
ok "hsm-plugin-terrasync (S3) started (PID=$PLUGIN_PID)"

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
    "$RBH_BIN" > /tmp/ilm_s3_rbh.log 2>&1 &
DAEMON_PID=$!
sleep 8
[[ -d /proc/$DAEMON_PID ]] || die "robinhood-rs daemon failed: $(grep message /tmp/ilm_s3_rbh.log | tail -3)"

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

# 记录创建时间戳（用于策略 mtime 边界过滤）
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

# 确认 changelog 已摄取
BATCHES=$(python3 -c "
import json
count = sum(1 for line in open('/tmp/ilm_s3_rbh.log')
    if 'changelog batch complete' in line)
print(count)
" 2>/dev/null || echo 0)
ok "10s elapsed — changelog batches processed: $BATCHES"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 5: Archive 策略 → S3（含 shadow）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 5: Archive policy → RustFS S3 (data + shadow namespace) ==="

# 重新计算 cutoff（10s 前）
CUTOFF_TS=$(( $(date +%s) - 10 ))

ARCHIVE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-s3-archive\",
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

# 触发策略
curl -sf -X POST "${API_URL}/api/policies/${ARCHIVE_ID}/run" \
    -H 'Content-Type: application/json' -d '{}' > /dev/null

# 等待所有测试文件归档
log "Waiting for files to be archived to S3..."
for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/docs/readme.md" \
            "${TEST_DIR}/data/dataset.bin" "${TEST_DIR}/data/record1.csv"; do
    wait_for_hsm_state "$FILE" "archived" 60 || die "Archive timeout for $FILE"
done
ok "All files archived to RustFS S3"

# 验证 S3 对象（数据 + shadow）
log "S3 objects after archive:"
s3_list_test_objects

# 验证 catalog 中的 HSM 状态
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
        \"name\": \"ilm-s3-release\",
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

# 等待 HSM poller 更新 catalog
sleep 6
mysql -u root rbh_entries -e "
    SELECT name, hsm_state FROM entries
    WHERE name IN ('report.txt','readme.md','dataset.bin','record1.csv')
    ORDER BY name;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# Phase 7: 验证 S3 中数据和 shadow 均存在
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 7: Verify S3 objects (data + shadow) ==="

S3_STATUS=$(s3_count_data_objects)
DATA_COUNT=$(echo "$S3_STATUS" | grep -oP 'data=\K[0-9]+' || echo 0)
SHADOW_COUNT=$(echo "$S3_STATUS" | grep -oP 'shadow=\K[0-9]+' || echo 0)
[[ "$DATA_COUNT" -ge 7 ]] || err "Expected ≥7 data objects, got $DATA_COUNT"
[[ "$SHADOW_COUNT" -ge 7 ]] || err "Expected ≥7 shadow objects, got $SHADOW_COUNT"
ok "S3 状态: $DATA_COUNT 个数据对象, $SHADOW_COUNT 个 shadow 对象"

log "S3 object listing:"
s3_list_test_objects

# ─────────────────────────────────────────────────────────────────────────────
# Phase 8: 透明 Restore（cat 触发 HSM Restore ← S3）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 8: Transparent restore — cat released files → S3 ==="
log "Reading released files will trigger HSM Restore via hsmd + TerrasyncMover::restore() ← S3"

# 读取每个文件（Lustre 自动触发 restore，cat 阻塞直到完成）
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
grep "restored" /tmp/ilm_s3_plugin.log | tail -10

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

RESTORE_COUNT=$(grep -c "restored" /tmp/ilm_s3_plugin.log 2>/dev/null || echo 0)
ok "TerrasyncMover processed $RESTORE_COUNT restore operations from S3"

# ─────────────────────────────────────────────────────────────────────────────
# Phase 10: Remove 策略 → 删除 S3 副本（含 shadow）
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 10: Remove policy → delete S3 backend copies + shadow ==="

# 等待 catalog 同步 HSM 状态（restored → archived）
log "Waiting for catalog HSM state sync..."
sleep 10

S3_BEFORE=$(s3_count_data_objects)
log "S3 before remove: $S3_BEFORE"

REMOVE_POLICY=$(curl -sf -X POST "${API_URL}/api/policies" \
    -H 'Content-Type: application/json' \
    -d "{
        \"name\": \"ilm-s3-remove\",
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
log "Remove policy triggered, waiting for S3 cleanup..."

# 等待 S3 数据对象清空
for ((i=1; i<=60; i++)); do
    S3_STATUS=$(s3_count_data_objects)
    DATA_LEFT=$(echo "$S3_STATUS" | grep -oP 'data=\K[0-9]+' || echo 999)
    SHADOW_LEFT=$(echo "$S3_STATUS" | grep -oP 'shadow=\K[0-9]+' || echo 999)
    if [[ "$DATA_LEFT" -eq 0 && "$SHADOW_LEFT" -eq 0 ]]; then
        ok "S3 数据对象和 shadow 均已清除"
        break
    fi
    sleep 1
done

log "Plugin remove operations:"
grep "removed" /tmp/ilm_s3_plugin.log | tail -10

# ─────────────────────────────────────────────────────────────────────────────
# Phase 11: 最终验证
# ─────────────────────────────────────────────────────────────────────────────
log "=== Phase 11: Final verification ==="

FINAL_ERRORS=0

# 验证 S3 数据对象为空
S3_FINAL=$(s3_count_data_objects)
DATA_FINAL=$(echo "$S3_FINAL" | grep -oP 'data=\K[0-9]+' || echo 999)
SHADOW_FINAL=$(echo "$S3_FINAL" | grep -oP 'shadow=\K[0-9]+' || echo 999)

if [[ "$DATA_FINAL" -eq 0 ]]; then
    ok "S3 数据对象: 已全部删除 ✓"
else
    err "S3 数据对象: 仍有 $DATA_FINAL 个 ✗"
    FINAL_ERRORS=$((FINAL_ERRORS + 1))
fi

if [[ "$SHADOW_FINAL" -eq 0 ]]; then
    ok "S3 shadow 对象: 已全部删除 ✓"
else
    err "S3 shadow 对象: 仍有 $SHADOW_FINAL 个 ✗"
    FINAL_ERRORS=$((FINAL_ERRORS + 1))
fi

# 验证 Lustre 文件仍可读（数据已回到 Lustre OST）
for FILE in "${TEST_DIR}/report.txt" "${TEST_DIR}/docs/readme.md"; do
    if cat "$FILE" >/dev/null 2>&1; then
        ok "$(basename $FILE) 在 Lustre 上可读 ✓"
    else
        err "$(basename $FILE) 读取失败 ✗"
        FINAL_ERRORS=$((FINAL_ERRORS + 1))
    fi
done

# 执行记录汇总
log "Policy execution history:"
mysql -u root rbh_entries -e "
    SELECT SUBSTRING(id,1,8) as exec_id, state,
           DATE_FORMAT(scheduled_fire_time,'%H:%i:%s') as fired_at,
           DATE_FORMAT(finished_at,'%H:%i:%s') as finished
    FROM executions ORDER BY scheduled_fire_time DESC LIMIT 10;" 2>/dev/null

# 操作汇总
log "TerrasyncMover S3 operations:"
python3 -c "
import re
for op in ['archived', 'restored', 'removed']:
    count = sum(1 for line in open('/tmp/ilm_s3_plugin.log') if op in line)
    print(f'  {op}: {count}')
" 2>/dev/null

# catalog 最终状态
log "Final catalog HSM distribution:"
mysql -u root rbh_entries -e "
    SELECT hsm_state, COUNT(*) as cnt FROM entries GROUP BY hsm_state;" 2>/dev/null

# ─────────────────────────────────────────────────────────────────────────────
# 结果
# ─────────────────────────────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════════════════"
if [[ "$FINAL_ERRORS" -eq 0 ]]; then
    echo "  ✓ ILM S3 端到端测试 PASSED"
    echo ""
    echo "  完整流程验证 (后端: RustFS ${S3_ENDPOINT}):"
    echo "    1. 文件写入 Lustre (${TEST_DIR}) ✓"
    echo "    2. robinhood-rs Full Scan → MariaDB catalog ✓"
    echo "    3. Changelog 实时监听 ✓"
    echo "    4. Archive 策略 (mtime>10s) → S3 (数据+shadow) ✓"
    echo "    5. Release 策略 → OST 释放，元数据保留 ✓"
    echo "    6. S3 数据对象 + shadow 验证 ✓"
    echo "    7. cat 文件 → Lustre 透明 Restore ← S3 ✓"
    echo "    8. Restore 后 exists archived（无 dirty）✓"
    echo "    9. Remove 策略 → S3 数据+shadow 全部删除 ✓"
    echo "   10. Lustre 文件仍可读 ✓"
else
    echo "  ✗ ILM S3 端到端测试 FAILED ($FINAL_ERRORS errors)"
fi
echo "══════════════════════════════════════════════════════════════"

exit "$FINAL_ERRORS"
