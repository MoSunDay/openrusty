Commit: d9a7ede
# e2e 演练扩展至覆盖全部功能点（d9a7ede）

## Context
- 集成演练此前为 48 checks，`kv-probe` 探针插件只覆盖部分阶段；`post_read`/`rewrite`/`access`/`body_filter`/`log`、插件内 `kv_scan`、`path:<n>` 键提取与节点容量上限、`OPENRUSTY_CONFIG` 启动等能力缺少端到端回归。

## Change Summary
- `plugins/kv-probe` 扩展为覆盖全部 8 个阶段的一方探针：新增 `post_read`/`rewrite`（含 418 拒绝）/`access`（含 403 拒绝）/`body_filter`（按块累计并回写响应头）/`log`（带 TTL 标记）处理器，以及 `kv_scan` 自检。
- `scripts/integration.sh` 由 48 扩展到 67 checks、sections 1-24：新增 status JSON 形状、前置三阶段、`body_filter`、`log`、`kv_scan`、`kv-scheduler` 的 `path:<n>` 提取 + `max_tasks_per_node` 上限（超限任务回落默认均衡）、`OPENRUSTY_CONFIG` 环境变量启动第二网关实例。
- `echo_upstream` example 与 README 同步更新（节点标识、演练说明）。

## Impact Surface
- `scripts/integration.sh` 成为覆盖全部功能点的权威验收基线：基础代理/流式协议、粘滞调度与 TTL、热重载全语义、健康检查与重试、失败收容（超时/内存/两种失败策略）、路由超时、ip_hash、kv-probe 全阶段。
- `kv-probe` 从此是阶段管线的常驻回归探针，而不仅是 content/KV 用例。

## Notes / Compatibility
- 演练在独立测试端口（18080/191xx）运行，不影响生产单元；无接口或行为变更。
- 记录理由：本次虽以测试为主，但同时包含 `kv-probe` 插件代码扩展，且确立了"67 checks 全功能覆盖"这一长期回归基线，具检索价值。

## Related Docs
- [WASM 插件阶段管线与沙箱](../../plugin-pipeline/index.md)
- [kv-scheduler 亲和调度](../../vllm-kv-scheduler/index.md)
- [运维与部署](../../ops/index.md)
- [网关请求管线](../../../agents/gateway-pipeline/index.md)
