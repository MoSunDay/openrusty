Commit: d9a7ede
# 负载均衡与健康检查

## 能力概述
- 每个 upstream 在多个 peer 之间分发流量，支持平滑加权轮询与客户端 IP 固定两种策略；被动健康检查自动摘除故障 peer 并在冷却后恢复。

## 触发方式
- 由转发路径自动驱动（每次 upstream 尝试前选一个健康 peer）；策略在 `config/openrusty.toml` 的 `[[upstreams]].balancer` 配置：`swrr` | `ip_hash`。
- 插件可在 `balancer` 阶段接管 peer 选择（如 [kv-scheduler 亲和调度](../kv-scheduler/index.md)）。

## 行为与规则
- `swrr`：平滑加权轮询，按 `[[upstreams.peers]].weight` 分配，避免突发集中。
- `ip_hash`：按客户端 IP 取模，将同一客户端固定到健康集合中的同一 peer。
- 被动健康：`fail_window_s` 窗口内累计 `max_fails` 次失败即将该 peer 标记 down；`fail_timeout_s` 后放行试探请求，成功则恢复。
- 选择始终在健康集合内进行；`GET /openrusty/status` 报告每个 upstream 的 `peers` 与 `healthy` 计数。

## 关键状态与异常
- 状态：每 (upstream, peer) 的失败计数、down 标记、恢复时间；按 upstream 名键控、跨热重载保留。
- 异常：全部 peer 不可用 → 转发失败；仅有单个健康 peer 时流量集中；`ip_hash` 在健康集合变化时会发生重映射。

## 关联逻辑模块
- [代理与负载均衡](../../agents/proxy/index.md)
- [网关请求管线](../../agents/gateway-pipeline/index.md)
