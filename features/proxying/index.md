Commit: 08a95ba
# 代理转发与流式协议

## 能力概述
- 客户端在同一端口上同时获得 HTTP/1.1 与 h2c（prior-knowledge HTTP/2）服务。
- 请求按路由转发到 upstream；WebSocket 连接端到端透传；SSE 与分块响应逐块流式回传（同时流经插件 `body_filter` 阶段）。
- 转发请求自动追加 `X-Forwarded-For`（保留并合并上游已有值）。

## 触发方式
- 任何命中 `[[routes]]` 中 `path_prefix` 的请求；路由按最长前缀匹配，未命中返回 404。
- WebSocket：带 `Upgrade` 头的请求被识别后切换为双向隧道。
- 路由与超时由 `config/openrusty.toml` 的 `[[routes]]`（`path_prefix`/`upstream`/`timeout_ms`）定义。

## 行为与规则
- 每条路由可设 `timeout_ms`（整请求超时，`0` 为禁用）；超时返回 502。路由超时默认**不**触发换 peer 重试（区别于连接级失败），直接返回 502。
- 连接级失败（建连/连接被拒等）会换一个健康 peer 重试，上限为 upstream 的 `retries`；响应一旦开始则不再重试。
- 路由可设 `retry_on_timeout`（默认 `false`）：为 `true` 时，路由超时会在当前 peer 上记录一次失败并换下一个 peer 重试，仍受 upstream `retries` 上限约束。注意重放语义：对推理类 POST 请求，超时重试可能在另一个 peer 上重新执行该请求（双重计算开销），因此默认关闭。
- 建连受 `connect_timeout_ms` 约束。
- 可选主动健康检查：在 `[upstreams.health.active]` 配置（`interval_ms` 默认 1000、`timeout_ms` 默认 1000、`path` 默认 `/`、`unhealthy_threshold` 默认 2、`healthy_threshold` 默认 2），存在该表即启用。探测为对每个 peer 的 `path` 发起短 HTTP GET，非 2xx 计为失败；连续失败 `unhealthy_threshold` 次标记为不健康，连续成功 `healthy_threshold` 次恢复。最终 peer 健康 = 被动失败状态 ∧ 主动探测结果；`/openrusty/status` 按 upstream 报告主动探测状态。
- h2c 与 HTTP/1.1 复用同一监听端口，无需分别配置。

## 关键状态与异常
- 状态：路由匹配结果、所选 peer、重试计数。
- 观测：`GET /openrusty/metrics` 暴露 Prometheus 文本格式指标：`openrusty_requests_total{route,code}`、`openrusty_request_duration_seconds`（固定桶覆盖 5ms..120s）、`openrusty_upstream_attempts_total{upstream,result}`、`openrusty_plugin_errors_total{plugin,kind}`、`openrusty_peer_healthy{upstream,addr}`、`openrusty_kv_entries{plugin}`。
- 异常：无匹配路由 404；全部 peer 不可用/重试耗尽 → 上游错误；路由超时 → 网关超时错误（默认不换 peer 重试；`retry_on_timeout=true` 时按 `retries` 上限换 peer 重试）；`balancer` 插件未选择时回落到配置的均衡算法。

## 关联逻辑模块
- [网关请求管线](../../agents/gateway-pipeline/index.md)
- [代理与负载均衡](../../agents/proxy/index.md)
