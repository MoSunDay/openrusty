Commit: 681ad49
# WASM 插件阶段管线与沙箱

## 能力概述
- 插件以独立 `.wasm` 模块形式参与请求生命周期的 8 个 nginx 对齐阶段：`post_read`、`rewrite`、`access`、`content`、`balancer`、`header_filter`、`body_filter`、`log`。
- 插件通过返回码（Decision）影响请求：继续、放行、短路应答、拒绝；并可通过 host KV 在阶段间/请求间共享带 TTL 的状态。

## 触发方式
- 插件从 `plugins.dir` 目录加载（`*.wasm`），执行顺序由 `plugins.order` 指定，未列出的按文件名排序随后执行。
- 阶段由网关自动驱动：请求侧四阶段 → 每次 upstream 尝试前的 `balancer` → 响应侧 `header_filter`/按块 `body_filter` → 收尾 `log`。
- 插件开发：`openrusty-sdk`（`no_std`）+ `openrusty-macros` 的 `#[phase(...)]`；ABI 契约见 `docs/wasm-abi.md`。

## 行为与规则
- Decision 语义（nginx 对齐）：`0`=Ok 已处理继续、`-5`=Declined 放行继续、`-4`=Done 停止阶段链（短路：模块经 `resp_body_set` 写了 body 则 `200 + body`，否则空 204；`body_filter` 中为流结束）、`100..=599`=以该状态码拒绝（可携带 body）。
- 沙箱：单次阶段调用受 `plugins.timeout_ms` 超时与 `plugins.max_memory_mb` 内存上限约束；越界即被中止，不会拖垮网关。
- 失败策略 `plugins.on_failure`：`fail_open`（默认）按放行继续，`fail_closed` 按 503 拒绝；每次失败计入该插件错误计数。
- 插件自由配置经 `[plugins.settings.<name>]` 以字符串键值传入。

## 关键状态与异常
- 状态：每插件错误计数（`GET /openrusty/status` 可见）、插件 KV（跨请求、跨热重载保留）。
- 异常：插件 trap/超时/协议错误按失败策略降级；KV 空值与不存在不可区分（插件不应存空值）。

## 关联逻辑模块
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)
- [网关请求管线](../../agents/gateway-pipeline/index.md)
