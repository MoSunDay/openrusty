Commit: 08a95ba
# 代理与负载均衡（openrusty-proxy）

## 职责
- 上游模型与转发层：upstream/peer 定义、负载均衡算法、被动+主动健康检查、连接池客户端、HTTP 转发与重试、WebSocket 字节隧道。

## 边界
- 负责：`swrr`/`ip_hash` 选择算法（`balancer.rs`）；被动+主动健康的记账与判定（`health.rs`）；按地址池化的客户端（`client.rs`）；转发、`X-Forwarded-For` 合并、可重试错误分类（`forward.rs`）；双向隧道（`tunnel`）。
- 不负责：阶段编排与 `balancer` 阶段的插件调用（[网关请求管线](../gateway-pipeline/index.md) 的 `pipeline_peer.rs` 把插件选择注入本层）；配置解析（`openrusty-core`）。

## 关键设计
- 均衡：`swrr` 为平滑加权轮询（`swrr_next` 维护 current weight）；`ip_hash` 按客户端 IP 在健康集合中取模固定（`ip_hash_pick`）。
- 被动健康：`max_fails` 次失败落在 `fail_window_s` 窗口内即标记 down；`fail_timeout_s` 后自动恢复试探；`record_success` 复位失败计数。`HealthRegistry` 按 (upstream 名, peer 下标) 键控，热重载时按名保留、不重置。
- 主动健康：`record_probe` 记录探测结果，`evaluate_active` 纯函数按 `unhealthy_threshold`/`healthy_threshold` 判定状态迁移；阈值只门控翻转（成功不把健康 peer 标脏、失败不治愈脏 peer）。探测任务本身在 `openrusty-server`（`active_probe.rs`）。
- 重试语义：仅连接级失败可重试（`is_retryable`），换一个健康 peer 重来，上限为 `upstreams.retries`；响应已开始后不重试。
- 连接：`ClientPool` 按 peer 地址池化 HTTP 客户端，建连受 `connect_timeout_ms` 约束。
- 流式：普通转发逐块搬运响应体；WebSocket 升级后 `tunnel` 双向透传字节流。

## 核心链路
1. 管线给出候选集合（健康过滤）→ 均衡算法或插件 `balancer` 阶段选定 peer。
2. 从连接池取客户端 → 注入 `X-Forwarded-For` → 转发请求。
3. 连接级失败 → `record_failure` → 在 `retries` 内换 peer 重试；成功 → `record_success`。
4. 响应头回传管线（`header_filter`），响应体逐块流回（`body_filter`）；WebSocket 则切入隧道。

## 依赖与接口
- 依赖 hyper/hyper-util、socket2、`openrusty-core`（upstream/route 配置类型）。
- 对外接口：`forward`、`tunnel`、`swrr_next`/`ip_hash_pick`、`HealthRegistry` 系列（`register`/`healthy_indices`/`record_failure`/`record_success`/`record_probe`）、`ClientPool`。
- 代码锚点：`crates/openrusty-proxy/src/{upstream,balancer,health,client,forward}.rs`。

## 关联模块
- [网关请求管线](../gateway-pipeline/index.md)
- [WASM 插件运行时](../wasm-runtime/index.md)（`balancer` 阶段可接管 peer 选择）
