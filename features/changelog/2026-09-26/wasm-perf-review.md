Commit: 53fc7c6
# WASM 方案性能审查（只读 review，代码零改动）

## Context
- 应对 wasm 批次优化排期做全面体检。基线锚点：or-kvprobe（8 阶段插件）≈535 µs/req vs 无插件管线 or-warn ≈74 µs/req（iteration-A 复测，paired delta 方法论），wasm 路径放大约 6–8 倍。
- 与 `docs/perf-review-2026-08-25.md` §2.1–2.3 对照：已知项仍未落地；另有一批本次新发现的 ABI 边界开销。

## Findings（按收益排序）
- **P0 实例化**：每请求每插件完整 `Store::new`+`linker.instantiate`（`runner.rs:54-101`），Engine 仅开 epoch_interruption（`registry.rs:64-68`），默认 on-demand mmap，无 `instantiate_pre`、无 PoolingAllocator、无磁盘编译缓存。改法：加载期构建 `InstancePre` 随快照发布 + `allocation_strategy(PoolingAllocator)`（需新增 pool 容量配置项）；实例池化后置。
- **P1 状态拷贝与阻塞**：每插件每阶段 push/pull 对 `ctx/peers/resp_headers` 深拷贝（`session.rs:167-187`），8 阶段×N 插件≈16N 次/请求，body_filter 每 chunk 重复；phase 同步调用跑在 tokio worker 上，`body_filter.rs:59-70` 在 `poll_frame` 持 session 锁同步调 wasm。改法：HostData 生命周期=单请求，push/pull 改 `mem::take` 所有权转移（深拷贝→0）；过渡 spawn_blocking、长期 async_support+call_async。
- **P2 ABI 边界（新发现）**：① 每次 host call 经 `caller.get_export("memory")` 按名重查 memory（`mem.rs:3-15`，PluginRt 缓存的句柄未被 linker 函数使用）；② two-phase 首探 256B（SDK `ffi.rs:182-183`）对 >256B 读取必失败→payload 构建×2；③ `req_meta("body")` 每次调用 `to_vec` 重建（`linker_req.rs:84`），无请求级缓存/分块 API；④ kv_scan 每条 3 次拷贝（`host_state.rs:204-253`）；⑤ `resp_header_set` 3 次拷贝+双份记录（`linker.rs:101-129`）。
- **P3 配套**：插件 profile `opt-level="s"`+lto（`plugins/*/Cargo.toml`）——实例化是热点时占优，P0 落地后需重测 opt-level=3；无 per-phase 延迟埋点（仅请求级直方图 `metrics.rs:16-21`），建议补实例化 vs 调用拆分指标。
- **不建议**：换 Winch（稳态吞吐 Cranelift 正确）、开 fuel（epoch 已覆盖）、shared-memory 提案（payload 小，复杂度不配收益）。

## Impact Surface
- **契约偏差（待修文档或补实现）**：`docs/wasm-abi.md:13-16` 声称 host 用 `orr_alloc` 做 host→guest 交接，实现中 host 从不调用它（`runner.rs:25-28` 标注 reserved；实际 two-phase 写入 guest 自备缓冲）。
- 实施顺序建议：P0 → P1a（所有权转移）→ P1b（卸载阻塞）→ P2 逐项；每步 `scripts/bench.sh` paired delta 验证。

## Related Docs
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)（性能特征节）
- `docs/perf-review-2026-08-25.md`、`docs/perf-iteration-a-2026-08-26.md`
