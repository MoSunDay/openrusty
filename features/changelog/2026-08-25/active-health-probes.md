Commit: 08a95ba
# 主动健康检查：周期探测全部 peer，与被动健康取交集

## Context
- 被动健康检查只对"有流量"的 peer 生效：一个不再接收请求的 peer 挂掉后永远不可见；Nginx / lua-resty-upstream-healthcheck 风格的主动探活缺失。
- 目标：可选配置下，网关周期性对 upstream 全部 peer 发短 HTTP GET，连续失败摘除、连续成功恢复，无流量 peer 也能被感知。

## Change Summary
- `crates/openrusty-core/src/config.rs`：新增 `ActiveHealthConfig`（`interval_ms`/`timeout_ms`/`path`/`unhealthy_threshold`/`healthy_threshold`，serde 带默认；校验拒绝 0 interval/timeout/threshold 与无前导斜杠的 path）；`[[upstreams]].health.active` 存在即启用。
- `crates/openrusty-proxy/src/health.rs`：新增 active plane——`record_probe` 记录探测结果，`evaluate_active` 纯函数按阈值判定状态迁移（阈值只门控翻转：成功不把健康 peer 标脏、失败不治愈脏 peer），`is_active_healthy`/`active_peers` 供查询；最终 `is_healthy = 被动 ∧ 主动`，未知 (upstream, peer) fail-open。
- `crates/openrusty-server/src/active_probe.rs`（新文件，178 行）：探测任务每 `interval_ms` 并发 GET 全部 peer 的 `path`，非 2xx / timeout / 转发错误计失败（WARN 日志带原因）；启动与热重载后按配置重建。
- `config/openrusty.example.toml`：注释示例。
- `scripts/integration.sh` §26：仅靠主动探测摘除 peer（无业务流量）并验证恢复。

## Impact Surface
- 配置：新增可选表 `[upstreams.health.active]`，缺省行为不变（不配置 = 纯被动）。
- 观测：`/openrusty/status` 的 upstream `healthy` 计入 active plane。
- 不影响：ABI、插件、路由语义。

## Notes / Compatibility
- 启动竞态缺陷（首探测周期全 peer 静默标脏）在同 commit 内修复，见 [active-health-boot-race](./active-health-boot-race.md)。

## Related Docs
- [负载均衡与健康检查](../../load-balancing/index.md)
- [代理与负载均衡](../../../agents/proxy/index.md)
