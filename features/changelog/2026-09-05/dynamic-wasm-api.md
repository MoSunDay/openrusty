Commit: 08a95ba
# 动态 WASM 执行 API：POST /api/v1/dynamic/{name}

## Context
- 需要把"一段 wasm 程序"当作 HTTP API 直接执行（策略计算、内联转换等一次性模块），不必进插件目录、不必走代理管线、不必 reload。本特性新增第 19 个 host import `resp_body_set` 打通模块写响应体的通道，再以 `[dynamic]` 配置节 + `DynamicRegistry` + 专用端点把能力暴露出来；插件 ABI 权威文档同步落盘（docs/wasm-abi.md "Dynamic execution API"）。

## Change Summary
### A. ABI：第 19 个 import `resp_body_set`
- **host 侧**（`openrusty-wasm/src/linker.rs`）：`resp_body_set(ptr, len) -> i32`，`>=0` = 接受字节数，`-1` = 拒绝（超 1 MiB 上限或指针不可读）；同请求内重复写覆盖前值。
- **SDK 侧**（`openrusty-sdk/src/host.rs`）：`host::set_resp_body(&[u8]) -> bool`，任意阶段可调。

### B. 代理管线：body 感知的 Done/Deny 短路（`resp_shortcut.rs`）
- `Decision::Done` + 已写 body → `200 + body`（不再恒空 204）；`Deny(s)` + body → `s + body`；模块自设响应头原样保留（content-length 除外），有 body 且模块未设 content-type 时补默认 `text/plain; charset=utf-8`；body-less 204 保持真正空体。

### C. `[dynamic]` 配置 + `DynamicRegistry`（`openrusty-wasm/src/dynamic.rs`、`openrusty-core/src/config/dynamic.rs`）
- 配置键：`dir`（env `OPENRUSTY_DYNAMIC_DIR` 覆盖并可单独启用）、`timeout_ms`(50)、`max_memory_mb`(16)、`on_failure`(fail_open)、`max_body_bytes`(1 MiB)、`[dynamic.settings.<name>]` 自由键值（键须匹配 `^[A-Za-z0-9][A-Za-z0-9._-]*$`）。
- registry 为 stat 驱动：编译缓存键 `(mtime_secs, mtime_nanos, size)`，替换文件下一请求即生效（无需 reload）；按名 singleflight 编译；`HostState`（KV + 错误计数）按模块名跨替换存活。缺节 = 特性完全关闭。

### D. 端点：`POST /api/v1/dynamic/{name}`（`openrusty-server/src/dynamic_api.rs`）
- body 先缓冲（超 `max_body_bytes` → 413）→ 合成单插件管线（post_read→rewrite→access→content→header_filter→log；无 balancer/body_filter/代理）→ `DynOutcome` 映射为响应。GET → 405；未知名 → 404；坏名（`..` 等）→ 400；全部结果计入 `openrusty_dynamic_requests_total{module,code}`（`/openrusty/metrics`，非空才渲染）。
- **路由挂载是启动期语义**：仅当启动配置含 `[dynamic]`（或 env）时挂载；reload 在该节变化时重建 registry，但无法向运行中的 router 挂/摘路由（`state::apply_dynamic`、`app::with_dynamic_routes`）。

### E. 示例插件（`plugins/dynamic-echo`、`plugins/dynamic-reverse`）
- dynamic-echo：content 阶段回显请求体（空体回退默认串），`[dynamic.settings.echo] greeting` 前缀拼接证明配置管道；自设 `x-dynamic` 与 `content-type` 证明模块头优先；另注册 access（Declined）与 log 阶段。dynamic-reverse：反转请求体字节，用于"替换文件 → 不 reload 即换行为"的 e2e。

## 测试覆盖
| 面 | 载体 |
|----|------|
| import 语义 | openrusty-wasm/src/{linker,dynamic_tests}.rs、openrusty-sdk/src/host.rs |
| registry 缓存/替换/singleflight/名校验 | openrusty-wasm/src/dynamic_tests.rs |
| 端点映射（200/204/413/404/400/405/头规则） | openrusty-server/src/dynamic_api.rs 内嵌测试 |
| 配置键与 env 覆盖 | openrusty-core/src/config/dynamic.rs |
| e2e | scripts/integration.sh §31（13 项：问候拼接、模块头、405/404/400/413、metrics、替换即生效、200 并发、reload 保活缓存、代理面不受影响） |
| 示例插件 | 两插件各自 host 侧单元测试（build-plugins.sh 一并跑） |

- 全量回归：`cargo test --workspace` → 498 通过；`scripts/build-plugins.sh` → 4 插件；`scripts/integration.sh` → 119/119。

## Impact Surface
- 默认关闭（无 `[dynamic]` 节 = 零路由成本）；启用后 `/api/v1/dynamic/` 前缀遮蔽同前缀代理路由，`dir` 可写者即网关上的 wasm 执行者（目录权限即信任边界），已在 config/openrusty.example.toml 注释中明示。
- 代理管线新增 body 感知短路：原 `Done` 恒 204 的行为仅在模块写了 body 时升级为 200，未写 body 的既有插件行为不变。

## Related Docs
- [docs/wasm-abi.md](../../../docs/wasm-abi.md)（"Dynamic execution API" 节，权威契约）
- [config/openrusty.example.toml](../../../config/openrusty.example.toml)（`[dynamic]` 全注释块）
- [插件管线](../../plugin-pipeline/index.md)、[WASM 运行时](../../../agents/wasm-runtime/index.md)

## Review Fixes（同日随审随修，随 `681ad49` 落盘）
- **P1 · 链式 body 交接**：`session.rs` 回收 HeaderData 时不再无条件覆盖 `resp_body` —— 改为仅当模块本轮真的写了 body 才回填；链上后续插件的 `Done` 不再丢弃前序插件已写出的响应体（新增 `chain_body_survives_later_done_plugin`）。
- **P2 · Log 期 body 写入的状态语义**：`dynamic.rs` 在 HeaderFilter+Log 收尾时若发现 `status == 204 && body 非空`，升级为 200（新增 `late_body_write_upgrades_204_to_200`）；nginx 对齐语义见 [docs/wasm-abi.md](../../../docs/wasm-abi.md) 新增段落。
- **P3 · 注释纠偏**：`dynamic_api.rs` 指标归因注释明确禁用态 404 与 router 405 不计入 `openrusty_dynamic_requests_total`。
