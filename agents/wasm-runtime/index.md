Commit: d9a7ede
# WASM 插件运行时（openrusty-wasm）

## 职责
- 基于 wasmtime 的插件运行时：编译/实例化插件、链接 `openrusty` 命名空间 imports、沙箱约束执行、热重载 registry、host 侧插件 KV 状态。

## 边界
- 负责：ABI 链接（`linker*.rs`：请求读取与 KV 操作的 host imports）；每阶段调用的沙箱执行（`runner.rs`）；ABI 校验（`registry_validate.rs`）；快照的原子发布与整体回滚语义（`registry.rs`）；按插件名的 host KV（`host_state.rs`）。
- 不负责：阶段在请求生命周期中的触发时机（[网关请求管线](../gateway-pipeline/index.md)）；ABI 的契约文本本身（权威文档 [docs/wasm-abi.md](../../docs/wasm-abi.md)）。

## 关键设计
- 一个 `.wasm` 文件 = 一个插件；`Module` 编译后缓存于快照，**每个请求以全新 `Store` 实例化**（内存上限按 store 施加）。
- 契约入口：guest 导出 `orr_on_phase(phase, ctx) -> i32` 与 `orr_alloc`（缺失 `orr_alloc` 在加载期校验失败）；返回值经 `Decision::from_abi` 解码（`0`=Ok、`-5`=Declined、`-4`=Done、`100..=599`=Deny，其余为协议错误）。
- 沙箱：单次阶段调用受 `plugins.timeout_ms`（wasmtime epoch interruption）与 `plugins.max_memory_mb`（`StoreLimits`）约束；trap/超时/协议错误按 `plugins.on_failure` 降级：`fail_open`（默认）→ `Declined` 放行，`fail_closed` → `Deny(503)`；错误按插件名计数并经 `registry.status()` 暴露。
- 热重载：`PluginRegistry::reload` 重新读配置、编译并校验全部插件，成功后以 `arc-swap` 原子发布新 `PluginSnapshot`；任一步失败整体拒绝、旧快照保留；`generation` 递增。
- host KV：键控于插件名（跨重载保留），支持带 TTL 的 `kv_get`/`kv_set`/`kv_del` 与 `kv_scan`；数据经两段式读写（`orr_alloc` 分配、长度不足返回 `-所需长度`）跨边界传递。
- guest 侧配套：`openrusty-sdk`（`no_std`：host imports 绑定、guest 分配器、`dispatch!`）与 `openrusty-macros`（`#[phase(...)]`）；一方插件见 `plugins/`。

## 核心链路
1. 启动：`bootstrap` 扫描 `plugins.dir`，按 `plugins.order` + 文件名排序编译校验，发布初始快照。
2. 请求期：管线对每个阶段调用 `run_phase` → 实例化/执行 `orr_on_phase` → 解码 Decision；失败走 `fallback_decision` 并计数。
3. 重载：新快照编译校验全部通过后原子替换；在途请求继续用旧快照直至完成。

## 依赖与接口
- 依赖 wasmtime、arc-swap、dashmap、`openrusty-core`。
- 对外接口：`PluginRegistry::bootstrap/reload/snapshot/status`、`run_phase`、host KV。
- 代码锚点：`crates/openrusty-wasm/src/{registry,registry_validate,runner,linker,linker_req,linker_kv,host_state,session,mem}.rs`。

## 关联模块
- [网关请求管线](../gateway-pipeline/index.md)
- [代理与负载均衡](../proxy/index.md)
- 契约：[docs/wasm-abi.md](../../docs/wasm-abi.md)
