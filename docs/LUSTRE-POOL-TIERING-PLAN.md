# robinhood-rs 内置 Lustre 在线冷热分层方案

robinhood-rs 默认提供通用 Robinhood 功能，策略引擎统一采用 `hsm.storage.io/v1alpha3` 资源模型。在线冷热分层是同一引擎内置的可选能力，按 Lustre 文件系统启用。分层策略决定哪些文件何时从源池迁到目标池，文件当前所在的池以实际 OST 布局为准。迁移通过 `liblustreapi` 完成，保留原路径和 FID。 [Lustre 2.16.1 `lfs-migrate(1)`](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/doc/lfs-migrate.1)

## 已确认的首版规格

| 决策 | 首版行为 |
| --- | --- |
| 配置入口 | `/api/policies` 接收 `hsm.storage.io/v1alpha3` `Policy` JSON 文档。通用策略与分层策略共用编译和调度流程。池迁移通过规则中的 `lustre.pool_migrate` 动作配置，每条分层策略关联一个 Lustre 文件系统和一个迁移方向。 |
| 配置资源 | 通过 REST 管理 `Policy`、`Selector`、`ExecutionProfile`、`AuthorizationPolicy`、`ApprovalPolicy` 和 `SystemSafetyPolicy`。服务端内置注册 `ActionContract`，REST 提供只读查询。 |
| 策略规则 | 定时评估。按源池、分类标签、`mtime` 静置时长及读取热度选择候选，实际条件和周期由操作方配置。支持按读取热度自动升温。 |
| 热度覆盖 | 使用显式配置的业务 Lustre 客户端。对依赖热度的规则，必要样本缺失或过期时暂缓，明确采到的 `0` 分参与评估。 |
| 迁移实现 | Rust 通过 FFI 直接调用 `liblustreapi`，文件数据由进程内的 POSIX I/O 复制。`lfs migrate` 用于对照行为。 |
| I/O 与并发 | 采用 `lfs migrate -b` 对应的阻塞迁移。单文件复制期间 Lustre group lock 会让该文件上的并发 I/O 等待。同一文件系统的不同 FID 可以并行迁移。并发受策略的文件系统并发上限和执行配置的全局槽位共同约束，同一个 FID 同时只允许一个迁移。单文件大小上限由操作方配置。 |
| 通用功能及 HSM | 验收通用 Robinhood 功能，以及原生 HSM 策略和池迁移按共存规则运行。 |

按源池容量调度时，达到高水位才开始处理，降至低水位便停止继续派发。两项判断均读取 Lustre 实时容量。原生 `Archive → Release → Restore` 全链路真机验收，以及固定升温时限由后续版本评估。 [阻塞迁移实现](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L1049-L1128)

## 控制台资源管理

控制台支持注册已有 Lustre 挂载。部署时为 daemon 配置托管目录和宿主机挂载权限后，由 daemon 执行托管文件系统的挂载、卸载和重启恢复。跨存储 HSM 卡片标为「暂未开放」。开发数据库从当前初始化 schema 创建。 [部署说明](../packaging/README.md#daemon-managed-lustre-mounts)

## 通用引擎与在线分层的接入边界

| 文件系统配置 | 运行行为 |
| --- | --- |
| 未配置在线冷热分层 | 运行 changelog、目录扫描与索引、查询报告和策略。原生 HSM 动作由文件系统的 HSM 能力与策略控制。 |
| 配置并启用在线冷热分层 | 提供同一组通用功能，并按分层策略运行 OST 池迁移。配置读取热度升温规则时，为该文件系统启动相应的热度采集及评估。 |

创建或修改分层策略时校验所选文件系统和池，按启用的策略注册采样和迁移任务。daemon 重启后从持久化的策略恢复这些任务。同一 daemon 管理多个文件系统时，各文件系统独立启用分层。验收覆盖通用部署启动、多文件系统混合启用，以及分层启用后通用策略与分层策略同时运行。 [现有多文件系统配置](../README.md#configuration)

建议复用每个 MDT 的 changelog 消费和游标，将 `HSM` 事件用于原生 HSM 状态更新，将 `LYOUT` 用于条带布局刷新。策略引擎复用候选查询、调度、预览、预算和按文件协调执行的能力。动作接口区分外部请求已受理和已确认完成，并由各动作提供权威的完成证据。原生 HSM 使用 Lustre HSM 状态和完成信号，池迁移使用重新打开 FID 后看到的实际布局。业务客户端采样的读取热度作为分层规则的输入。 [现有事件接口](../crates/rbh-change-source/src/lib.rs#L141) · [现有 Archive 请求](../crates/rbh-actions/src/executor.rs#L170)

原生 HSM 与在线分层可以在同一文件系统同时运行。池迁移仅处理尚未归档、没有活动 HSM 操作的在线普通文件。已归档但仍在线的文件、已释放文件以及正在执行 HSM 操作的文件继续由原生 HSM 策略管理。执行池迁移前以 Lustre 实时状态复核资格。查询失败或无法确认时暂缓，并记录原因。验收至少覆盖普通在线文件可迁移、已归档但仍在线的文件跳过、候选被选中后开始 HSM 操作的文件暂缓。 [共存规则 ADR](adr/0001-lustre-online-pool-tiering.md)

本文件记录的 Lustre API 实测覆盖在线分层功能。通用 Robinhood 功能、策略入口和原生 HSM 动作通过回归用例验收。

## 通过 REST API 管理策略

`POST /api/policies` 与 `PUT /api/policies/{id}` 的请求体采用策略 JSON。`apiVersion` 为 `hsm.storage.io/v1alpha3`，`kind` 为 `Policy`，资源内容位于 `metadata` 和 `spec`。通用策略和分层策略统一进行 JSON Schema 校验、资源引用解析和语义编译，再交给同一调度器。不符合策略格式的文档返回校验错误。 [现有策略资源路径](../crates/rbh-api/src/handlers.rs#L36)

在 `spec.target.filesystem` 指定文件系统，在 `spec.trigger` 配置定时触发，在 `spec.rules.entries[]` 配置条件和动作。池迁移动作使用 `action.type: lustre.pool_migrate`、`targetRef: current.object` 和显式的源池、目标池参数。实现时为池迁移注册 `ActionContract`，为布局池名和读取热度注册 `ConditionExpr` 属性。首版分层用例采用 `first_match` 和周期评估，由编译器明确拒绝超出首版运行能力的组合。

策略编译时解析执行、系统安全、授权和审批资源。`preview` 只产生评估结果，`enforced` 通过对应授权和安全校验后执行。

| REST 操作 | 分层策略行为 |
| --- | --- |
| `POST /api/policies` | 验证、编译并创建一条策略。首次部署可用 `spec.enabled: false` 保存，或通过 `spec.execution.activation.mode: preview` 预览。 |
| `GET /api/policies`、`GET /api/policies/{id}` | 查询当前保存的策略、所属文件系统与启用状态。 |
| `PUT /api/policies/{id}` | 修改规则或启用状态，更新现有调度。 |
| `POST /api/policies/{id}/run` | 手动运行已保存的策略，按 `spec.execution.activation.mode` 预览候选或调度动作。 |
| `DELETE /api/policies/{id}` | 删除策略并撤销该策略的后续调度。 |

建议停用或删除时停止派发后续迁移，已开始交换的操作继续完成核对。通过 REST 保存、预览、启用、更新和停用策略，以及重启后恢复已保存的策略，均纳入端到端验收。 [现有创建与更新](../crates/rbh-api/src/handlers.rs#L120) · [现有试运行入口](../crates/rbh-api/src/handlers.rs#L169)

## 配置资源的 REST 管理

可写配置资源使用 `apiVersion: hsm.storage.io/v1alpha3`，并包含 `kind`、`metadata` 和 `spec`。集合路径提供 `GET` 和 `POST`，按资源名寻址的路径提供 `GET`、`PUT` 和 `DELETE`。`Policy` 的路由见上一节。

| Kind | 建议的集合路径 | 解析方式 |
| --- | --- | --- |
| `Selector` | `/api/selectors` | 策略的 `target.where` 或规则条件通过 `selectorRef` 引用。 |
| `ExecutionProfile` | `/api/execution-profiles` | 策略通过 `spec.execution.profileRef` 引用。 |
| `AuthorizationPolicy` | `/api/authorization-policies` | 策略通过 `spec.governance.authorizationRef` 引用。 |
| `ApprovalPolicy` | `/api/approval-policies` | 策略通过 `spec.governance.approvalPolicyRef` 引用。 |
| `SystemSafetyPolicy` | `/api/system-safety-policies` | 引擎按资源作用域自动匹配并合并限制。 |

`ActionContract` 由服务端与实际执行器一起注册。通过 `GET /api/action-contracts` 列出可用合同，通过 `GET /api/action-contracts/{name}` 查询合同详情。策略编译按 `action.type` 解析合同并校验参数，合同提供实际执行器支持的目标、前置条件和完成证据。

服务端使用可信调用方身份授权资源写操作，并记录修改者和资源修订版。策略编译绑定已解析的资源修订版。修改配置资源生成新修订版，已启动的运行及操作保留已绑定的修订版用于执行记录和对账。建议在删除仍被启用策略引用的资源时返回引用冲突。已开始的操作保留必要的历史证据。 [策略资源接口 ADR](adr/0002-policy-resource-api.md)

资源验收覆盖可写资源的 REST 增删改查、无效字段与循环引用的拒绝、未授权写入的拒绝和被引用资源的删除冲突。合同验收覆盖服务端注册、列表与详情查询，并验证 REST 的合同写请求被拒绝。运行验收覆盖资源修订后新建与在途运行的修订版绑定，以及 `enforced` 激活的系统安全策略匹配和授权决定。

## 实验环境

2026-11-27 CST，现场确认服务端和 node-120 客户端工具均为 Lustre 2.16.1，文件系统为 `lustre`，MGS 在 node-120，唯一的 MDT `lustre-MDT0000` 在 node-121。实际落点按 OSS 分组，两组使用同类 SAN 存储，适合验证策略和数据流向。

| 池 | OST 成员 | 活动 OSS | 测试目录默认布局 |
| --- | --- | --- | --- |
| `lustre.hot` | `OST0000`–`OST0007` | node-120 (`10.128.135.120`) | `.rbh-tiering-lab/hot`，1 条带 |
| `lustre.cold` | `OST0008`–`OST000f` | node-121 (`10.128.135.121`) | `.rbh-tiering-lab/cold`，1 条带 |

已在 MGS 创建两组池，并从 node-120 和 node-121 客户端分别校验成员。两个测试目录位于 `/mnt/smartiq/lustre/.rbh-tiering-lab`，父目录权限为 `0700`。文件系统根目录仍使用原有默认条带布局。建池后客户端需要等待配置传播。 [Lustre 运维手册第 23.2.1 节](https://doc.lustre.org/lustre_manual.pdf)

以下为该环境的建池命令，已执行。建池后的成员检查可重复运行。

```bash
# 在活动 MGS node-120 执行
lctl pool_new lustre.hot
lctl pool_add lustre.hot 'OST[0-7]'
lctl pool_new lustre.cold
lctl pool_add lustre.cold 'OST[8-f]'

# 在已挂载的客户端检查
lctl pool_list lustre.hot
lctl pool_list lustre.cold
lfs getstripe -d /mnt/smartiq/lustre/.rbh-tiering-lab/hot
lfs getstripe -d /mnt/smartiq/lustre/.rbh-tiering-lab/cold
```

| 实测 | 结果 |
| --- | --- |
| `lfs migrate -b -p … -c 1` 双向迁移 | 8 MiB 随机文件走 `hot/OST 2 → cold/OST 13 → hot/OST 3`。FID、inode、路径、硬链接、大小、权限、UID/GID、mtime 和 SHA-256 一致。`lfs hsm_state` 为 `0x00000000`。 |
| Python FFI 直接调用 `liblustreapi.so.1` | 约 6 MiB、带头部数据和尾部空洞的稀疏文件走 `hot/OST 1 → cold/OST 12 → hot/OST 2`。FID、硬链接、纳秒级 mtime 和 SHA-256 一致，迁移后仍为 208 个 512 字节块。两次交换后，旧源 fd 上 `llapi_layout_get_by_fd` 仍读到原池。关闭迁移 fd 后按同一 FID 重新打开，读到目标池和预期 OST。 |
| `lustre-MDT0000` changelog | 以临时 reader 的 `CREAT,CLOSE,LYOUT` mask 验证。冷热目录里新建文件都出现在同一事件流，迁移前属于 `hot` 的文件产生 `12LYOUT`，事件 49 的目标 FID 与源文件一致。 |
| 双客户端读取热度 | 用两个临时 Lustre 客户端挂载，对同一个冷池 FID 启用 `file_heat`。node-120 连续做 32 次 4 KiB `pread` 后，直接调用 `llapi_heat_get` 得到 `readsample=25`、`readbyte=104960`。同期 node-121 对同一 FID 的读热度仍均为 `0`。临时挂载的 `heat_period_second=60`、`heat_decay_percentage=80`。 |

测试样本、临时 reader 和热度测试专用挂载已清理。复查时 node-121 的默认 changelog mask 为 `MARK`，现有 `cl1` 无独立 mask。现场验证覆盖建池、双向迁移、changelog 观测及读取热度的客户端可见范围。应用策略在实现后进行端到端验收。

## `lfs migrate` 调用的 Lustre 用户态 API

以下基于与现场版本对应的上游 `v2_16_1`。普通文件的 OST 迁移入口 `lfs_migrate()` 位于 `lfs.c`。它负责协调多个接口。公共头文件里的 `llapi_migrate_mdt()` 对应 MDT 迁移。 [CLI 分派](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L4634-L4651) · [公开头文件](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/include/lustre/lustreapi.h#L485-L494)

| 步骤 | `lfs migrate` 2.16.1 的调用 |
| --- | --- |
| 获取源 MDT | 以可写方式打开源文件，调用 `llapi_file_fget_mdtidx`。默认使用 `O_DIRECT`，支持 `-D` 使用缓冲 I/O。 [源码](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L678-L704) |
| 创建目标布局 | 普通 `-p/-c/-S` 路径用 `llapi_file_open_param`，复合布局路径用 `llapi_layout_file_open`。两条路径都在源 MDT 上创建 Lustre volatile file，源 inode 保留在原路径。 [源码](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L705-L776) |
| 复制和落盘 | `llapi_file_is_sparse`、`llapi_data_seek` 探测空洞，POSIX `pread`、`pwrite`、`ftruncate`、`fsync` 负责数据传输和落盘，目标 fd 继承源 atime/mtime。 [复制源码](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L830-L1047) |
| 默认阻塞模式 | `llapi_group_lock` → `llapi_get_data_version(src, &dv, 0)` → 复制 → `llapi_fswap_layouts_grouplock(src, dst, dv, 0, 0, SWAP_LAYOUTS_CHECK_DV1)` → `llapi_group_unlock`。持 group lock 时，数据版本查询用 `flags=0`，避免与 OST extent lock 冲突。 [源码](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L1049-L1128) |
| `--non-block` 模式 | `llapi_lease_acquire(..., LL_LEASE_RDLCK)`，复制期间 `llapi_lease_check`，按 `LL_DV_RD_FLUSH` 前后比对源数据版本，再读取目标版本，提交 `llapi_fswap_layouts(..., SWAP_LAYOUTS_CLOSE)`。放弃复制时释放 lease。 [复制和版本检查](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L1130-L1198) · [交换分支](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L1435-L1476) |

Rust 执行器通过 FFI 直接调用现场验证过的 `llapi_open_by_fid` 和 `llapi_create_volatile_param`。后者是公开的 volatile file 接口，内部用 `llapi_file_open_param` 应用条带参数，临时 inode 在最后一个 fd 关闭后回收。用 `llapi_get_poolmembers` 校验池成员，并通过 `llapi_layout_get_by_fd`、`llapi_layout_pool_name_get`、`llapi_layout_ost_index_get` 校验实际布局。按各接口的返回约定处理 FD、空指针、`-errno` 及 `-1` 加 `errno`。 [volatile 和交换实现](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/liblustreapi_swap.c#L110-L283) · [FID API](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/include/lustre/lustreapi.h#L521-L547) · [池查询 API](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/include/lustre/lustreapi.h#L168-L174)

## 项目内的接入方案

1. **由 MDT changelog 驱动目录更新。** 复用 `lustre-changelog` 与 `rbh-change-source` 的 MDT 事件流和 ACK。部署时先在 node-121 为 robinhood-rs 注册专用 reader，给该 reader 配置现有目录同步所需的 create、close、truncate、rename、unlink 等事件，并加入 `LYOUT`。mask 接受 `LYOUT` 这个名字。建立 reader 后做基线扫描，再从它的起点消费。池判定来自按 `(filesystem_id, FID)` 读取的实时 layout，目录路径只提供默认条带策略。目录和条带写库成功后提交 checkpoint，数据搬迁由策略 worker 执行。 [现有消费与提交](../crates/rbh-daemon/src/changelog.rs#L243) · [Lustre changelog API](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/include/lustre/lustreapi.h#L599-L608)
2. **补齐 layout 的增量刷新。** 当前 `stat_entry_by_fid` 在建目录项时读取布局，`Layout` 分支直接跳过，`Close` 的常规 restat 主要刷新文件属性。收到 `LYOUT` 时刷新池名、条带尺寸、数量与 OST 清单。在 `CREATE` 尚无实际布局的情况下，后续写关闭或首个有效布局事件再补齐。新增按 FID 的局部更新，在同一事务更新池名与条带明细，保留 namespace edge 和 `sm_status`。事件重放时查实时布局，迁移自身产生的 `LYOUT` 也走这条收敛路径。 [当前事件分支](../crates/rbh-daemon/src/changelog.rs#L798) · [初始布局采集](../crates/rbh-daemon/src/changelog.rs#L1470) · [现有条带事务](../crates/rbh-entry-store/src/store.rs#L1133)
3. **注册 `lustre.pool_migrate` 动作。** 通过 ActionContract 定义源池、目标池、可选条带数量和大小，以及实际布局复查作为完成证据。编译时校验 Lustre 布局能力、源目标池存在且互异。调度时按编译后的候选条件缩小范围，执行前用实时属性复验。内置动作走 `rbh-actions` 的文件系统级执行接口和有界 worker，通过 `lustre-api` FFI 执行。策略预览报告候选数量和数据量。 [候选查询和后端分派](../crates/rbh-policy/src/task.rs#L145)
4. **按分层策略选择方向和候选。** 每条策略指定一个源池和一个目标池。可分别配置 `hot → cold` 和 `cold → hot`，复用同一执行器。定时策略每轮根据配置评估标签、静置时间和热度条件。文件达到静置时间后会进入下一轮评估。双向策略同时启用时，建议用每文件最短驻留时间和分开的升降温阈值抑制来回迁移。 [现有策略模型](../crates/rbh-policy/src/model.rs#L20) · [现有触发器解析](../crates/rbh-policy/src/trigger_parser.rs)

## 读取热度与自动升温

Lustre 2.16.1 的 `llapi_heat_get` 从文件描述符所属客户端的内存 inode 读取 `readsample` 和 `readbyte`。开启 `file_heat` 后，有效文件读取更新这些随时间衰减的分值，客户端的快速缓存读取也经过计量。两个客户端对同一 FID 可以得到不同分值，且 inode 回收会清掉本地热度。策略字段应注明是衰减后的热度分值，并说明参与采集的客户端覆盖范围。 [Lustre 读写计量](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/llite/file.c#L1713-L1742) · [快速读取路径](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/llite/file.c#L2096-L2124) · [热度读取](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/llite/file.c#L4448-L4461) · [用户态 API](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/liblustreapi_heat.c#L38-L56)

在配置的业务客户端挂载上采集，由 robinhood-rs 按 `(filesystem_id, FID, client_id)` 保存最近采样时间和分值。首版可以按策略查出有界的一批候选 FID，让各参与客户端通过 `llapi_open_by_fid` 和 `llapi_heat_get` 采样。策略只用采用同一衰减配置、仍在有效期内的最新观测。依赖热度的规则遇到必要样本缺失或过期时暂缓，明确采到的 `0` 分正常参与评估。迁移 worker 在未参与采集的独立 Lustre 挂载上执行，因为迁移的复制读取本身也会增加读取热度。验收记录实际采样范围、升温延时和采集开销。 [Lustre 2.16.1 热度参数](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/llite/lproc_llite.c#L1796-L1897)

## 执行器语义与验收

- **迁移前。** 按文件系统和 FID 领取任务。以实时 fd 检查文件类型、当前池、目标池成员、条带模式、大小、策略条件及 HSM 状态。第一版面向同 FS 的单组件 RAID0 普通文件，源条带数和大小默认沿用，保留 POSIX 硬链接和稀疏文件。DoM、PFL、FLR、foreign、加密文件及带活动 HSM 操作或已有 HSM archive 状态的文件返回明确的处理原因。空文件单独覆盖布局存在与尚未物化两种场景。迁移遵守文件系统和全局并发限制、单文件大小上限及单轮执行预算。目录仅用于限定扫描范围，目录默认 Pool 布局保持原样。 [Lustre 迁移源码](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L1049-L1128) · [MDT 的 HSM 属性处理](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/mdd/mdd_object.c#L2646-L2710)
- **迁移中。** 目标 volatile file 放在同一 MDT，按需要匹配源 UID/GID。获取 group lock 后重读源信息及版本，复验候选条件，复制并持久化目标数据。交换前验证目标 layout 的池名及实际 OST。交换沿用版本检查。用 RAII 管理 layout、fd 和锁，支持在复制块之间协作取消。worker 的等待超时后保留任务占用，直到阻塞调用退出或完成核对，避免在首个迁移还运行时又提交一次。 [源码](https://github.com/lustre/lustre-release/blob/v2_16_1/lustre/utils/lfs.c#L748-L776) · [现有后端超时调度](../crates/rbh-policy/src/task.rs#L645)
- **迁移后。** 关闭迁移 fd，重新按 FID 打开，验证目标池、条带参数和实际 OST，再以局部事务刷新目录并记录完成。遇到 `EAGAIN`、忙、失败位置不明或写库失败，下一次先读实际布局，已达目标则补写目录。文件已删除或策略不再匹配则结束任务。用有界退避处理可重试错误，单独上报权限、池配置、空间不足和不支持的布局。多 worker 或多进程时共用 `(filesystem_id, FID)` 的任务占用，恢复时以实时布局核对执行结果。

### 首版验收用例

1. **通用功能。** 启动没有分层策略的 daemon，验证扫描、changelog 写库、查询报告及原生 HSM 策略入口。分层采集与迁移任务只为配置并启用相应策略的 Lustre 文件系统注册。在同一 daemon 中配置两个文件系统，其中仅一个启用分层，验证各自的策略运行范围。
2. **REST 与运行状态。** 通过 REST 创建、预览、启用、修改、停用和删除 `Policy`，验证运行调度与保存状态一致。预览时文件布局保持原样，重启后调度按持久化的策略恢复。校验格式不符的策略文档、无效引用、无效池、相同源目标池、未注册文件系统和不支持的后端。
3. **降温与升温。** 从无法调用 `lfs` 的执行环境运行 daemon，保留 `liblustreapi` 及 Lustre 客户端挂载。热池文件定时命中降温规则后迁到冷池 OST。通过已配置的业务客户端读取，热度达到规则阈值后，用同一个 FID 自动迁回热池。两次迁移均验证路径、硬链接、校验和、实时 OST 布局，并检查 `LYOUT` 重放后的索引。纳入不符合策略、样本缺失与过期、实际采到 `0` 分以及迁移读取隔离的对照。
4. **共存与失败恢复。** 在分层启用时验证原生 HSM 策略的现有行为。分层先检查已归档但仍在线、已释放及活动 HSM 操作的文件，并覆盖候选入选后 HSM 状态变化。另测并发写、重命名、删除、目标池空间不足及交换前后进程退出。核对重复运行或数据库更新失败时，按 FID 和实时布局补齐状态。
5. **同文件系统并行迁移。** 在独立测试数据库和带 `hot`、`cold` 池的 Lustre 挂载上运行控制台验收。两个测试文件在限速下同时进入派发状态，结束后逐个核对目标池、FID 和内容。 [运行说明](../tests/README.md#scoped-pool-migration)
