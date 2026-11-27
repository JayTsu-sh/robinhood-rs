# Rev.8 切换验收

按 [#22](https://github.com/JayTsu-sh/robinhood-rs/issues/22) 和[父规格 #14](https://github.com/JayTsu-sh/robinhood-rs/issues/14) 验收。环境为 Lustre 2.16.1、MariaDB 11 隔离实例和本地 daemon。使用专用 MDT reader、热池 `hot`、冷池 `cold`、独立业务客户端挂载和临时 POSIX copytool。测试策略只匹配临时文件的精确大小。运行 daemon 时用 seccomp 禁止 `execve` 和 `execveat`，真实布局由测试进程独立读取。

## 验收结果

| 场景 | 结果 |
| --- | --- |
| 从 `f89cd08` 升级 | 已启用的旧定义从 REST 列表消失，旧 ID 的读取、手动运行和删除返回 404。旧调度由 `Active` 转为 `Paused`。旧定义保留，原名称可用于新建 Rev.8 策略。既有 Rev.8 修订版及写入身份仍可查询。 |
| 唯一策略入口 | 旧格式的 POST 和 PUT 返回 HTTP 400 并指明 `hsm.storage.io/v1alpha3`。带旧 `target` 的手动运行请求返回 HTTP 400。无效池和当前部署外的文件系统在保存前被拒绝。 |
| 无分层策略的通用部署 | REST 扫描完成后可按 FID 查询。创建、重命名、解除链接和外部布局变化由 MDT `lustre-MDT0000` 的 changelog 更新目录。验证了作用域内的 Top Size 和目录用量报告。启用的 Rev.8 Archive 经临时 copytool 完成，REST 有操作及实时 HSM 状态证据。 |
| 并行配置两个文件系统作用域 | 同一 daemon 注册两个 Lustre 作用域，两边都部署采样配置。仅配置了启用升温策略的作用域启动采样，另一个作用域保留目录功能。删除最后一条升温策略后采样停止。升级前的 HSM 热度预览也未触发采样。 |
| 按策略双向迁移 | REST 预览返回 1 个候选、131087 字节，预览期间布局仍在热池。启用降温后重启，由保存的周期和修订版触发迁移。文件在最短驻留期和业务读取阈值满足后自动升温。 |
| 实际布局与文件身份 | FID `0x200000bd9:0x3e:0x0` 从热池 OST 3 到冷池 OST 10，再回到热池 OST 5。两个方向均核对了实时布局的池名和实际 OST，并检查了 inode、原路径、硬链接、稀疏分配及 SHA-256。 |
| 与原生 HSM 共存 | 同一 daemon 完成原生 Archive、Release、Restore。已归档但在线、已释放的文件因 `hsm_state_ineligible` 暂缓。有活动 Archive 的文件因 `hsm_action_active` 暂缓。三者的布局都保持原样，同批符合资格的在线文件进入冷池。另在已选中 FID、锁定前发起 Archive，原迁移操作复核后因 `hsm_action_active` 暂缓，FID、布局和内容一致。 |
| 布局交换前进程退出 | 对原 FID 的实时核对确认源池仍有效，操作标为 `swap_not_observed_in_source_pool`。后续周期再次校验并正常降温，FID 和内容一致。 |
| 布局交换后进程退出 | 重启后核对实际冷池布局，补齐原操作和目录记录。REST 历史只有一次尝试，FID 和内容一致。 |
| 交换后目录更新失败 | MariaDB 触发器拒绝公布冷池布局后，操作保持 `unknown`，实时文件已在冷池。删除策略并重启后，原操作转为 `completed`，目录更新到目标池，只发生一次迁移。 |
| 单文件和并发限制 | 超过单文件上限的候选在尝试前暂缓。第一个文件持有迁移占用期间，同一文件系统的另一条策略未开始第二次迁移，前一个操作退出后第二个才开始。 |
| 并发修改、重命名和删除 | 在源文件加锁前从另一客户端写入、改名或解除最后一个链接。三项均因 `source_generation_changed` 暂缓。保留文件的实时 FID 与内容符合业务操作的结果，已解除链接的文件未恢复。 |
| 目标池空间不足 | 在目标 volatile file 创建边界注入 `ENOSPC`，REST 记录交换前失败。原池、FID 和内容保持原样。 |
| 读取热度与重放 | MariaDB 和真实 Lustre 的 REST 集成用例覆盖零热度、缺失或过期样本、客户端独立观测和真实周期升温。上一阶段 [#21](https://github.com/JayTsu-sh/robinhood-rs/issues/21) 真机验收中，必需采集端离线使先前达标的 FID 在提交前以 `heat_client_unavailable` 暂缓，恢复后同一修订版完成升温。迁移复制和执行器核对读取没有提高业务客户端的零热度。布局集成用例在真实迁移后重放 `LYOUT`，断言数据库失败时游标不确认，重放后目录池名及 OST 正确。 |

## 验收边界

两个 Lustre 作用域使用同一个物理文件系统的不同客户端挂载。该环境用于确认配置、采样和目录按作用域隔离，独立物理文件系统同时迁移仍需对应的测试环境。空间不足通过注入 `ENOSPC` 验证，现有 OST 容量配置未调整。Archive、Release 和 Restore 使用临时归档目录和独立 archive ID，属于原生 HSM 路径的验收。
