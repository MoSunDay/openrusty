Commit: 08a95ba
# kv-probe 补 balancer 阶段探针；wasm-abi 文档对齐实现

## Context
- kv-probe 覆盖 7/8 阶段，唯 balancer 缺探针；`docs/wasm-abi.md` 若干描述与实现有偏差（peer 视图语义、`balancer_set_peer` 返回码、`body_filter` 语义、路由先行时序）。

## Change Summary
- `plugins/kv-probe/src/lib.rs`：新增 `ProbeMode::Balancer` 与 `#[phase(balancer)]` 处理器（`/probe?mode=balancer`）——纯校验 balancer 阶段的 peer 视图（恰好 3 peer、全部 healthy、字段非空），然后 `set_peer(0)` 固定 peer；resp-header / req_meta 探针沿用既有机制带出断言结果。
- `docs/wasm-abi.md` 修正：`req_peer_count`/`req_peer_get` 暴露路由 upstream 的**全部** peer（healthy 与否，健康位为请求开始时快照），`req_peer_get` 越界返回 `-2`（不写 TLV）；`balancer_set_peer` 返回 `-1` 仅表越界（不做健康检查）；`body_filter` 为 observe-only + 末次空块 `last=true`；路由先于一切插件阶段（404 时不跑任何阶段含 log）。
- `scripts/integration.sh` §28：balancer 阶段 pin 验证 + resp header/req_meta 断言。

## Impact Surface
- 插件（演示用途）：kv-probe 新增一种探针模式，wasm 产物需经 `scripts/build-plugins.sh` 重建。
- 文档：wasm-abi.md 为权威契约，本次仅纠偏描述、不改 ABI 本身（编号/导入/返回码值不变；`req_peer_count` 语义澄清为 total）。

## Related Docs
- [WASM 插件契约](../../../docs/wasm-abi.md)
- [WASM 插件运行时](../../../agents/wasm-runtime/index.md)
