Commit: fa110bf
# 迭代 A 性能修正与配置面（round-3 报告 docs/perf-iteration-a-2026-08-26.md）

## Context
- round-2 基准显示：访问日志 +29.9µs/req、client 池无空闲回收（fd 只增不减）、每请求 metrics 双锁+二次路由。本轮吸收 N2/N3/N8 与 §1.3/§1.5 快赢项。

## Change Summary
- 日志（A1）：writer 改 `tracing_appender::non_blocking(stdout())`，guard 由 main 持有至退出 flush；**`server.log_level` 默认值 info→warn**；`finish_log` 去 per-request `path.clone()`。
- 配置面（A4）：新增 `[server] http1_only`（默认 false）。true 时纯 HTTP/1.1（保留 WebSocket upgrade），跳过 h2c preface 探测。
- client 池（A3）：per-peer hyper client 注册 `.pool_timer` + `.pool_idle_timeout`（新配置 `[upstreams] pool_idle_timeout_ms`，默认 60000）；boot/reload 时按 peers 预建 client，未知 addr 懒创建回退保留。修复空闲连接永不回收的 fd 泄漏。
- metrics（A2）：`record_request_timed` 单临界区合并请求计数与直方图；`handle_request` 返回 `(Response, Option<route_idx>)`，fallback 不再二次路由匹配；暴露格式零变化。
- 运维（A5）：gateway systemd 单元加 `LimitNOFILE=65535`；被动健康成功/失败记录锚定请求起点时刻。

## Impact Surface
- **行为变化**：默认日志级别 warn——依赖请求级访问日志的部署需显式 `log_level = "info"`（example.toml 已注释说明）。
- 新配置项：`[server] http1_only`、`[upstreams] pool_idle_timeout_ms`；均有 serde default，旧配置零改动可跑。
- 实测：访问日志成本 paired delta 29.4→20.9 µs/req；20k 短连接后 fd 数恒定。

## Related Docs
- [代理转发与流式协议](../../proxying/index.md)、[运维与部署](../../ops/index.md)
