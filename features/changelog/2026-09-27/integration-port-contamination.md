Commit: bee611a
# 集成演练"稳定失败"根因：GATE_PORT 环境污染（代码零改动）

## Context
- WASM 性能优化轮次（2026-09-26）与干净基线 worktree 都稳定复现 `scripts/integration.sh` 173/174，唯一失败为第 23 节 `capped tasks spread over all 3 nodes`（`30_plugins_probe.sh`），当时按"存量基线问题"搁置。

## Findings
- 元凶：本机常驻 agent 进程以 ~2s 周期轮询默认网关端口 `127.0.0.1:18080/api/nodes/channel`。第 23 节把 extract 切到 `path:1` 后，该 path 首段 `api` 成为 task key；每次轮询续期 `aff:api`（TTL 6s > 2s 轮询周期 → 永远存活），在 `max_tasks_per_node=1` 下提前占掉一个节点的槽位。
- 失败链条：pk-a/pk-b 正常避开已占节点，pk-c 面对三节点全满 → `choose_least_loaded` 无解 → Declined → 回落 SWRR（此为既有语义，见 [vllm-kv-scheduler](../../vllm-kv-scheduler/index.md)）→ 三探针只落 2 节点 → 扩散断言失败；sticky 与 fallback 断言不受影响，与全部观察吻合。2s×6s 的周期关系使失败近似必现，"基线同样失败"由此而来。
- 诊断方法（可复用）：临时在插件 balancer 阶段 `host::log` 打点（task、affinity 命中、各节点 active 计数、pick），复跑后从 `gate.log` 还原每次调用看到的 KV 视图。

## Resolution
- 代码/契约/配置零变化；换未被轮询的端口复跑 `GATE_PORT=18099 ./scripts/integration.sh` → **174/174 全绿**。
- 运行规则：本机默认端口存在后台流量时，用 `GATE_PORT` 覆盖默认值（`scripts/integration.sh` 在 `GATE_PORT` 定义处有注释指引）。

## Related Docs
- `docs/perf-optimization-2026-09-26.md` §7.1（完整复盘）
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)、[vllm-kv-scheduler 亲和调度](../../vllm-kv-scheduler/index.md)
