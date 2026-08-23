Commit: d9a7ede
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
- 每条路由可设 `timeout_ms`（整请求超时，`0` 为禁用）；超时返回 502。
- 连接级失败（建连/连接被拒等）会换一个健康 peer 重试，上限为 upstream 的 `retries`；响应一旦开始则不再重试。
- 建连受 `connect_timeout_ms` 约束。
- h2c 与 HTTP/1.1 复用同一监听端口，无需分别配置。

## 关键状态与异常
- 状态：路由匹配结果、所选 peer、重试计数。
- 异常：无匹配路由 404；全部 peer 不可用/重试耗尽 → 上游错误；路由超时 → 网关超时错误；`balancer` 插件未选择时回落到配置的均衡算法。

## 关联逻辑模块
- [网关请求管线](../../agents/gateway-pipeline/index.md)
- [代理与负载均衡](../../agents/proxy/index.md)
