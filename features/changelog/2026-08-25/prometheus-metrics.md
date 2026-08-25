Commit: 08a95ba
# /openrusty/metrics：Prometheus 文本暴露端点

## Context
- 观测此前只有 `/openrusty/status` JSON 快照，无时序指标；运维需要请求量/耗时分布/上游尝试/插件错误/peer 健康的抓取端点。

## Change Summary
- `crates/openrusty-server/src/metrics.rs`（新文件，256 行）：进程内计数器与固定桶直方图（5ms..120s 覆盖推理时长），`record_request`/`record_duration`/`record_attempt`。
- `crates/openrusty-server/src/metrics_render.rs`（新文件，288 行）：Prometheus text exposition 0.0.4 渲染——`openrusty_requests_total{route,code}`、`openrusty_request_duration_seconds`（bucket/sum/count）、`openrusty_upstream_attempts_total{upstream,result}`、`openrusty_plugin_errors_total{plugin,kind}`、`openrusty_peer_healthy{upstream,addr}` gauge、`openrusty_kv_entries{plugin}` gauge；label 值转义。
- `crates/openrusty-server/src/app.rs`：`GET /openrusty/metrics` 路由先于代理 fallback；fallback 处按匹配路由记录每请求 code/duration（未匹配记 `route="unknown"`）。
- `crates/openrusty-wasm/src/registry.rs`：`record_error` 细分 kind（trap/timeout/bad_code，与 server 侧常量对齐），新增 `error_kinds()` 与 `kv_len()`；`metric_view()` 聚合输出。
- `scripts/integration.sh` §29：验证暴露形状（HELP/TYPE、route/code 标签、peer gauge、kv gauge）。

## Impact Surface
- 新端点：`GET /openrusty/metrics`，`text/plain; version=0.0.4`；不鉴权（与 status/reload 同策略，仅内网暴露）。
- 不影响：代理语义、ABI。

## Related Docs
- [运维与部署](../../ops/index.md)
- [网关请求管线](../../../agents/gateway-pipeline/index.md)
