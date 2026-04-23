# robinhood-rs vs robinhood (C) — 功能缺口分析与 P0 实施记录

> 最初分析日期：2026-04-23
> 基础版本：C 版 `/root/lustre/robinhood`（74k 行）vs Rust 版 `/root/rust/github/robinhood-rs`（分析时 8.1k 行）

## 目录

1. [现状覆盖概览](#现状覆盖概览)
2. [缺失模块清单（按目录对照）](#缺失模块清单按目录对照)
3. [小缺失 / 运维类](#小缺失--运维类)
4. [优先级分组](#优先级分组)
5. [P0 详细说明](#p0-详细说明)
6. [P0 实施结果](#p0-实施结果)
7. [遗留与后续方向](#遗留与后续方向)

---

## 现状覆盖概览

**Rust 已实现**：

| 领域 | crate | 状态 |
|---|---|---|
| Lustre FFI（FID / stripe / HSM / MDT / changelog） | `lustre-api` | 基本完整 |
| Changelog 监听 + 去重 + 批处理 | `lustre-changelog` | 核心完整（多 MDT 并行支持） |
| 实体目录（FID→元数据、硬链接、removed_entries） | `rbh-entry-store` | MariaDB 实现完整 |
| 谓词树（SQL pushdown + 内存 eval） | `rbh-predicate` | 完整，含 `OnOst` EXISTS JOIN |
| 初始扫描 | `rbh-fs-scan` | 完整 |
| REST API（policy CRUD / entries count / entries query / reports / scans） | `rbh-api` | ~10 端点 |
| 策略模型 + scheduler-rs 触发（时间 + 阈值） | `rbh-policy` | 触发 / 调和 / LRU / TargetFilter / ignore_fileclass |
| 基本动作执行器 | `rbh-actions` | Purge / HsmArchive / HsmRelease |

整体覆盖占 C 版 **15–20%**，核心"changelog → DB → 时间/阈值触发 → 动作"通道通了。

---

## 缺失模块清单（按目录对照）

### 1. 动作 / 策略模块（C `src/modules/` 12 个）

| C 模块 | 功能 | Rust 状态 |
|---|---|---|
| `backup.c` (2827 行) | 备份 policy + `rbhext` 外部工具集成 | ❌ |
| `lhsm.c` (1142 行) | 完整 HSM（archive/release/remove/restore tracking/hints） | ⚠️ 仅 archive/release 简化版 |
| `alerter.c` | 告警 policy（邮件/脚本） | ❌（`Alert` 枚举项存在、执行器未实现） |
| `checker.c` | 文件内容校验（checksum） | ❌ |
| `modeguard.c` | 权限监控 / 自动修正 | ❌ |
| `shook.c` | shook 绑定 | ❌ |
| `common_actions.c` | copy/move/hardlink/sendmail/log/cmd 通用动作 | ❌ |
| `common_sched.c` | 公共调度器 | ⚠️ 语义不对等 |
| `sched_ratelimit.c` | 动作级速率限制 | ❌ |
| `basic.c` | 基础 state manager | ❌ |
| 自定义脚本/外部命令动作 | `action = cmd(...)` DSL | ❌ |

Rust `ActionParams` 已含 `max_count / max_volume / timeout / nb_threads / lru_sort`；C 版还支持 pre/post 命令、重试、软/硬阈值、post-sched hooks、预过滤、classes。

### 2. 命令行二进制族（C `src/robinhood/`）

| C 二进制 | 功能 | Rust 状态 |
|---|---|---|
| `rbh_find` (1797 行) + printf 扩展 | 扩展 find，Lustre 属性 / `-printf` | ⚠️ 已实现核心子集（`rbh find`） |
| `rbh_report` (3192 行) | 报告（topdirs/topusers/topsize/dump/OST） | ⚠️ 已实现子集（`rbh report`） |
| `rbh_du` | Lustre-aware du | ❌ |
| `rbh_diff` | 扫描与 DB 差异 | ❌ |
| `rbh_import` | CSV/备份目录导入 | ❌ |
| `rbh_rebind` | FID 绑定变更 | ❌ |
| `rbh_recov` | 灾难恢复 | ❌ |
| `rbh_undelete` | 恢复 `removed_entries` | ❌ |
| `cmd_helpers.c` (守护进程模式) | `--scan/--check-thresholds/--run=policy/--dry-run/--target=...` | ⚠️ 阈值已做，其他运行模式未实现 |

### 3. 守护进程特性（C `rbh_daemon.c` 1934 行）

| 特性 | Rust 状态 |
|---|---|
| 多 MDT 并行 changelog | ✅（`RBH_MDTS`） |
| `--dry-run`、`--once`、`--target=...` 运行模式 | ❌ |
| `--check-thresholds`、条件触发 | ✅（`thresholds.rs`） |
| 高/低水位动态触发 | ⚠️ 高水位已有，**低水位停止条件尚未接入到 in-run 逻辑** |
| 配置 reload（SIGHUP） | ✅ |
| PID 文件 / systemd notify | ⚠️ systemd unit 有，notify 未接线 |
| `--diff` 日志输出 | ❌ |
| 孤儿 OST 对象工具（`ost_fids_remap` 等 `src/tools/`） | ❌ |

### 4. 策略语义（C `src/policies/` 共 12.2k 行）

| C 功能 | Rust 状态 |
|---|---|
| `policy_loader.c` — 文本 DSL + include / 继承 | ❌（Rust 改用 DB + JSON） |
| `policy_matching.c` — fileclass 分类、multi-rule、`ignore`/`ignore_fileclass` | ✅（`ignore_fileclass` 内嵌 `FileClassDef`，SQL 层 `AND NOT (...)`） |
| `policy_run.c` — 候选集、LRU 排序、并行分发、重试、进度追踪 | ⚠️ LRU + max_count 已做；并发 worker、软/硬阈值、重试未实现 |
| `policy_triggers.c` — OST/pool/user/group 占用率触发 | ⚠️ 已做 count/volume 阈值；**OST statfs 实际占用率未接入**（现按 DB 聚合） |
| `status_manager.c` — 状态机抽象 | ❌ |
| `policy_sched.c` — 策略级调度器插件 | ❌ |

### 5. 报表 / 查询 / 统计

- 按 user/group/class/ost/pool 聚合 — ✅ 基本子集
- topdirs / topusers / topsize — ✅
- OST 使用率、条带分布报表 — ❌
- 增量差异 — ❌
- size-profile / by-count/avgsize/szratio、split-user-groups — ❌

### 6. 配置 & 兼容

| 项 | 状态 |
|---|---|
| `src/cfg_parsing/` yacc/lex 文本配置解析 | ❌（设计上由 REST JSON 取代） |
| `doc/templates/*.conf` 语义移植 | ❌ |
| 类似 `scripts/cfg_25to30.sh` 的迁移脚本 | ❌ |
| `rbh-config` shell 管理工具 | ❌ |
| systemd unit / init 脚本 | ✅（`packaging/systemd/`） |
| RPM spec | ❌（C 版 `robinhood.spec.in` 未移植） |

### 7. 数据库 / 存储

| C 功能 | Rust 状态 |
|---|---|
| MySQL + SQLite 双后端 | ⚠️ 仅 MariaDB/MySQL |
| `listmgr_recov.c` — 灾难恢复元数据 | ❌ |
| `listmgr_stripe.c` — stripe 表 + 条带级查询 API | ⚠️ 数据有、按 OST 过滤已通过 `OnOst` 谓词，但条带分布统计 API 未做 |
| `listmgr_tags.c` — 扫描批次 tag（孤儿清理） | ❌ |
| `listmgr_iterators.c` — 可恢复大结果集游标 | ❌ |
| `update_params.c` — 动态参数存储 | ❌ |
| schema 版本迁移 | ⚠️ 仅初始化 |

### 8. 扫描 `src/fs_scan/`

| C 功能 | Rust 状态 |
|---|---|
| 任务栈 / 任务树并发控制 | ⚠️ 有但更简单 |
| 增量扫描（mtime 阈值跳过） | ❌ |
| 扫描进度持久化 / 续扫 | ❌ |
| 跳过 fileclass / 忽略路径规则、`.rbh_ignore` | ❌ |

### 9. 可观测性 / 运维

| C 功能 | Rust 状态 |
|---|---|
| `rbh_logs.c` — 分级日志 + 邮件告警 + 日志轮转 | ⚠️ tracing JSON + logrotate 片段 |
| entry_proc_tools.c 管道统计 | ❌ |
| Prometheus / metrics 导出 | ❌（OTLP v2 已预留） |

### 10. Web GUI

C 版 `web_gui/` 目录（PHP）。Rust 无前端。

---

## 小缺失 / 运维类

1. DNE rename stitching — 多 MDT 合流窗口。
2. Changelog 用户自动注册（C 版 `rbh-config`）。
3. `rbhext_tool` 外部备份适配器 C/S 协议。
4. `uidgidcache.c` — UID/GID 名字缓存。
5. 硬链接策略语义（某路径删另一路径保留）。
6. 完整测试套件 — C 版 `tests/` 20+ 场景；Rust 只有 crate 单元测试 + 少量 changelog 集成测试。

---

## 优先级分组

### P0（让守护进程能替代 C 版基本场景）— ✅ 已完成

- 动态阈值触发
- OST / pool 级 targeted purge
- `ignore_fileclass` 实现
- `rbh_find`（子集）
- `rbh_report`（子集）
- 多 MDT
- SIGHUP reload
- systemd 单元

### P1（HSM 生产可用）

- 完整 `lhsm.c` 语义（hints、restore 跟踪、archive/release 重试与统计）
- `common_actions`（copy/move/sendmail/cmd）
- 动作速率限制
- 策略运行期软/硬水位（fire 后循环检查 low 停止）

### P2（报表 & 运维）

- `listmgr_reports` 完整报表 API
- `rbh_report` 剩余功能（size-profile / by-count/avgsize、status-info、maintenance）
- Prometheus metrics
- 配置 `.conf` → JSON 迁移工具
- `rbh_undelete` / `rbh_diff`
- 增量扫描 / 续扫 / `.rbh_ignore`

### P3（扩展 / 灾难恢复）

- `backup` 模块 + `rbhext_tool`
- 灾难恢复（`listmgr_recov`）
- SQLite 后端
- Web GUI
- 扩展检查器（checksum / modeguard / shook）
- RPM spec

---

## P0 详细说明

### 1. 动态阈值触发

基于 C 版 `policy_triggers.c`（2255 行）。

- **触发类型**：`TRIG_ALWAYS`（定时运行，已实现）vs `TRIG_CONDITION`（定时**检查条件**）。
- **条件 target**：`TGT_FS | TGT_OST | TGT_POOL | TGT_PROJID | TGT_USER | TGT_GROUP | TGT_FILE | TGT_CLASS`。
- **阈值维度**：`PCT_THRESHOLD`（百分比）/ `VOL_THRESHOLD`（字节）/ `COUNT_THRESHOLD`（条目）/ `CNTPCT_THRESHOLD`（inode %）。
- **水位**：`high_threshold`（启动）/ `low_threshold`（停止）/ `post_trigger_wait`（冷却）/ `alert_hw|lw`。
- **限额**：`max_action_nbr / max_action_vol`。

### 2. OST / pool 级 targeted purge

C 版 `policy_triggers.c::targeted_run()` + `policy_run.c`。

关键能力：
- 按 OST 过滤候选集（`stripe_items` JOIN）。
- 按 pool_name 过滤。
- 按 OST 个体统计"清了多少"，降到 lw 停止。
- 按 OST 使用率排序 OST（先清最满）。
- user/group/projid/class 限定。
- 按 LRU 排序候选。
- `--target=ost:3 --target-usage=low:70` 一次性运行。

### 3. `ignore_fileclass`

完整 fileclass 系统由命名条件集构成：
```
fileclass hot_data { definition { last_access < 1d } }
fileclass project_a { definition { tree == "/lustre/projA" } }
define_policy purge {
    ignore_fileclass = project_a, hot_data;
    ...
}
```
P0 版（已做）简化：`FileClassDef { name, predicate }` 内嵌在 PolicyDef，SQL 层 `WHERE scope AND NOT (p1 OR p2 ...)`。

### 4. `rbh_find`

POSIX `find(1)` + Lustre 扩展：`--ost/--pool/--projid/--class/--status/--lsost/--lsclass/--lsstatus/--printf/--exec`。

### 5. `rbh_report`（基本子集）

`--activity / --fs-info / --{user,group}-info / --top-users / --top-size / --top-dirs / --oldest-files / --deferred-rm / --dump / --dump-{user,group,ost,status}`。

### 6. 多 MDT

每 MDT 独立游标 / changelog user；并行 listener；故障隔离；DNE rename stitching 推迟。

### 7. SIGHUP reload

- `tokio::signal::unix` 监听。
- `arc_swap::ArcSwap<Config>` 原子替换。
- `tracing_subscriber::reload::Handle` 改日志级别。
- SIGUSR1 dump stats；SIGTERM/SIGINT 优雅关闭。

### 8. systemd 单元

- `rbh-daemon.service` + `rbh-daemon@.service`（多实例）。
- sysconfig / tmpfiles.d / logrotate。
- `ExecReload=/bin/kill -HUP $MAINPID`。
- `NoNewPrivileges / ProtectSystem=strict / LimitNOFILE=65536`。

---

## P0 实施结果

所有 9 项任务完成，136 个单元测试通过，`cargo check --workspace` 无错误。

| # | 任务 | 交付 | 测试 |
|---|---|---|---|
| P0.7 | SIGHUP 热重载 | `rbh-observability::Guard::reload_filter`、`rbh-daemon/signals.rs`、axum `with_graceful_shutdown` | obs 2 个测试 |
| P0.8 | systemd 单元 | `packaging/{systemd,sysconfig,tmpfiles.d,logrotate}` + README | — |
| P0.6 | 多 MDT | `pair_mdts_with_users` + `RBH_MDTS`；每 MDT 独立 listener；错误隔离 | 7 个测试 |
| — | REST 通用查询 | `POST /api/entries/query`；`SortKey` / `OrderDir` 白名单；`query_page` / `count_where` / `aggregate_by` | — |
| P0.4 | `rbh find` | 子命令：`--user/--group/--projid/--pool/--type/--name/--size/--mtime/--atime/--ctime/--sort/--asc/--limit/--json`；find(1) 时间/大小语法 | 11 个测试 |
| P0.5 | `rbh report` | `top-size / top-users / top-groups / fs-info / oldest`；REST `/api/reports/*` | — |
| P0.3 | ignore_fileclass | `FileClassDef { name, predicate }`；`compose_scope_with_ignores` | 3 个测试 |
| P0.2 | OST/pool purge | `Predicate::OnOst` + EXISTS JOIN；`TargetFilter`；`LruSortAttr` | 8 个测试 |
| P0.1 | 阈值触发器 | `TriggerSpec::ThresholdCount/Volume` + `ThresholdTarget`；`rbh-daemon/thresholds.rs` 周期轮询 | 2 个测试 |

### 新增环境变量

| 变量 | 用途 | 默认 |
|---|---|---|
| `RBH_MDTS` | 逗号分隔 MDT 列表 | *空* |
| `RBH_CHANGELOG_USER` | 单值或 CSV | *空* |
| `RBH_MDT_NAME` | 单 MDT 兼容 | *空* |
| `RBH_THRESHOLD_TICK_SECS` | 阈值轮询粒度 | 30 |
| `RBH_LOG` | env-filter 指令（SIGHUP 可重载） | `info` |
| `RBH_LISTEN_ADDR` | REST 监听 | `0.0.0.0:8080` |
| `RBH_DATABASE_URL` | MariaDB URL | `mysql://root@127.0.0.1/rbh_entries` |
| `RBH_LUSTRE_MOUNT` | 挂载点 | `/lustre` |

### 新增 REST 端点

| 方法 | 路径 | 用途 |
|---|---|---|
| POST | `/api/entries/query` | 通用谓词查询（分页 + 排序 + 可选 total） |
| POST | `/api/reports/aggregate` | 按列聚合（count/size） |
| GET | `/api/reports/top-size?n=N` | 最大 N 文件 |
| GET | `/api/reports/oldest?n=N` | 最久未访问 N 条 |

### 新增信号

- SIGHUP — 重读 `RBH_LOG`，重载 tracing 过滤器。
- SIGUSR1 — dump 运行时快照（hook 可扩展）。
- SIGTERM / SIGINT — 优雅关闭（cancel token → axum graceful shutdown → listener drain）。

---

## 遗留与后续方向

### P1 要点（直接基于 P0 成果）

1. **阈值的 low watermark 闭环** — 现在只用 high；fire 后让 PolicyRunTask 执行中周期性检查 low 并提前退出。
2. **完整 lhsm 动作** — 接入 HSM archive_id/hints/restore 跟踪 + 重试；当前 executor 是简化版。
3. **动作速率限制** — `sched_ratelimit.c` 语义：每秒最多 N 个动作、总带宽上限。
4. **OST statfs 实际占用率** — 目前 ThresholdVolume 是按 DB SUM(size)；真要复刻 C 版得走 `llapi_obd_statfs` 拿 `f_blocks/f_bfree`。
5. **并发 worker** — PolicyRunTask 现在串行跑候选；按 `nb_threads` 并行。

### 其他 follow-ups

- `rbh_find --printf` 格式符 / `--exec`。
- `rbh_report --size-profile / --dump-* / --maintenance`。
- `rbh_undelete` / `rbh_diff` CLI + 对应 REST。
- `.rbh_ignore` + 增量扫描。
- Prometheus `/metrics` 端点。
- 迁移工具：C 版 `.conf` → Rust JSON PolicyDef。
- 集成测试套件（挂载真 Lustre 或 testfs fixture）。

### 风险与注意事项

- **阈值 SUM(size) 不等于 Lustre `df`**：对多条带文件，`SUM(size)` 低估实际 OST 占用（因为单文件跨多个 OST）。用户须知，必要时转向真 `statfs` 实现。
- **OnOst 依赖 `stripe_items` 全量**：初始扫描和 changelog 必须完整记录所有条带，否则 targeted purge 会漏。
- **单 `cl` 共享多 MDT**：`RBH_CHANGELOG_USER=cl3` + `RBH_MDTS=m1,m2` 会给每个 MDT 用同一 user id — Lustre 不接受同 user 在多 MDT 注册，需用 CSV 形式 `cl3,cl4`。
