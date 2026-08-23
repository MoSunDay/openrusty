Commit: d9a7ede
# kv-scheduler 亲和调度

## 能力概述
- 一方插件（`plugins/kv-scheduler`，独立 wasm workspace）：vLLM 风格的 KV-cache 亲和调度。携带同一 task key 的请求粘滞到同一上游节点，命中缓存；新任务分配给当前活跃任务最少的节点。

## 触发方式
- 在 `balancer` 阶段生效（需在 `plugins.order` 中列出）；task key 从请求 URL 提取，规则由 `[plugins.settings.kv-scheduler].extract` 配置：
  - `query:<param>` —— query 参数；
  - `path:<n>` —— 从 1 开始计数的路径段。

## 行为与规则
- 粘滞：已有亲和记录（`aff:<task>`，TTL = `affinity_ttl_s`，默认 300）且目标节点健康时，续期记录并沿用该节点。
- 新任务：统计各节点存活任务数（`kv_scan` 亲和记录），选择活跃任务最少者；并列时取最近一次调度时间最旧的节点。选定后写入亲和记录与 `sched:<idx>` 调度时间。
- 容量上限：`max_tasks_per_node`（`0` = 不限）；达到上限的节点不参与选择。
- 记录不健康、无可用节点、或所有节点都达上限时，插件放行（Declined），回落到 upstream 配置的默认均衡算法。
- 释放：无需显式注销，亲和记录按 TTL 到期自动释放任务槽位；`log` 阶段仅输出信息日志。

## 关键状态与异常
- 状态：插件 KV 中的 `aff:<task>`（peer 下标）与 `sched:<idx>`（最近调度时间），均为 ASCII 十进制字符串，跨热重载保留。
- 异常：`extract` 未配置或请求不含 task key → 不参与调度；亲和节点被健康检查摘除 → 重新选节点。

## 关联逻辑模块
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)
- [代理与负载均衡](../../agents/proxy/index.md)
