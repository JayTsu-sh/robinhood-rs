# 统一策略资源 REST API

通用 Robinhood 策略与在线分层策略统一采用 `hsm.storage.io/v1alpha3` 资源模型。操作方通过 REST 管理 `Policy`、`Selector`、`ExecutionProfile`、`AuthorizationPolicy`、`ApprovalPolicy` 和 `SystemSafetyPolicy`。策略编译将这些资源的引用解析为已确定的修订版。格式不符的策略请求返回校验错误。

`ActionContract` 随服务端内置动作注册，通过 REST 提供只读查询。这样策略可引用已注册的动作，动作参数、前置条件和完成证据由实际执行器及其合同定义。在线分层使用这个统一接口配置 `lustre.pool_migrate`。 [首版规格](../LUSTRE-POOL-TIERING-PLAN.md)
