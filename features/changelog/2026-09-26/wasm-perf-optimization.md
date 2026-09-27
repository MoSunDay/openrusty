# WASM 性能优化落地(P0/P1a/P2 + per-phase 指标; P1b 实测回退)

## Context
- 实施同日只读审查 [wasm-perf-review.md](./wasm-perf-review.md) 列出的优化项。基线: or-kvprobe(8 阶段插件)约 535 us/req vs 无插件管线 or-warn 约 74 us/req, wasm 路径放大约 6-8x(`docs/perf-iteration-a-2026-08-26.md`, paired-delta 方法论)。
- 完整报告(每项的位置/运维面/风险): `docs/perf-optimization-2026-09-26.md`。

## Changes
- **P0 实例化**: 加载期 `Linker::instantiate_pre` 生成预解析计划随快照发布(`LoadedPlugin.pre`), 每请求仅在新 `Store` 上执行计划; 引擎启用 wasmtime pooling allocator, 新配置 `plugins.instance_pool_size`(默认 1000, 0 = on-demand 回退; 槽内存按 `max_memory_mb` 预留, MiB 取整到 64 KiB 页、压在 4 GiB 下)。槽耗尽 = 实例化失败 -> `on_failure` 策略。容量按 插件数 x 峰值并发 估算。
- **P1a 状态拷贝**: 阶段间 push/pull 改 `mem::take` 所有权转移(`session.rs`), ctx/peers/resp_headers/req_body/body_chunk 深拷贝降为 0(原 8 阶段 x N 插件每请求重复克隆)。
- **P1b block_in_place 过渡已实现→实测回归→回退**(数据见 perf 文档 §3): async handler 内同步 wasm 调用一度经 `tokio::task::block_in_place` 卸载, paired-delta 实测两条路径同时回归(or-warn 80→156 µs/req; or-kvprobe 11.5k→8.6k rps)——每次调用 ~10 µs 的 scheduler run-queue handoff × 每请求 8 阶段, 对亚毫秒 phase 占主导; 已完整回退(helper `wasm_offload.rs` 删除, 调用点恢复 inline `session.run_phase`), 后续路径为 wasmtime `async_support(true)` + `call_async`。
- **实测结果(c=100, paired-delta, baseline=HEAD 53fc7c6 两轮一致)**: or-kvprobe 11.5–12.0k → **19.4k rps(+~65%)**, p50 7.9–8.4 → 4.84 ms(−~40%), CPU 396–421 → 202.5 µs/req(−~50%); or-warn 28.5–29.8k → 35.0k rps(63.1 µs/req); wasm 边际成本 ~341 → ~139 µs/req(−~59%)。
- **P2 ABI 边界**: ① guest `Memory` 句柄实例化时缓存进 `HostData`, imports 不再每调用 `get_export("memory")`; ③ `req_meta("body")` 改借用 refcounted `Bytes` 的 `Cow`(标量键同), 容量探测也不再付整份 body 拷贝; `kv_get` 从 DashMap entry 借用直写 guest(两次拷贝 -> 一次); `resp_header_set`/`del` clone 削减(语义不变)。
- **观测**: 新直方图 `openrusty_plugin_phase_seconds{phase}`(每请求每阶段累计墙钟时间, 恰好一次观测, 桶 25us..1s); 短路经 `finish_log`、流式 body 经 `run_log`、websocket 经 `run_ws_log` 三路单点记录; wasm 侧配套 `RequestSession::take_phase_stats()`。
- **文档纠偏**: `docs/wasm-abi.md` 原声称 host 用 `orr_alloc` 做所有 host->guest 交接, 与实现不符(实为 two-phase guest 自备缓冲); 已改为如实描述, `orr_alloc` 保留为加载期必检导出 + 未来 host-push 预留。

## Impact Surface
- 配置面: `plugins.instance_pool_size` 新增(见 `config/openrusty.example.toml` 注释); 槽容量不足的新失败模式已收编进 `on_failure` 语义并计入插件错误计数。
- 指标面: `/openrusty/metrics` 新增 `openrusty_plugin_phase_seconds{phase}` 指标族(ops/proxying 特性文档已同步)。
- 契约面: ABI 导入/导出签名零变化; 每请求仍是全新 `Store` 与新实例(隔离语义不变)。
- 验证: `cargo test --workspace` / `scripts/build-plugins.sh` / `scripts/integration.sh`; 收益复测用 `scripts/bench.sh` paired delta(主判据 or-kvprobe - or-warn)。

## Related Docs
- `docs/perf-optimization-2026-09-26.md`(本轮完整报告)
- [wasm-perf-review.md](./wasm-perf-review.md)(同日只读审查)
- `docs/perf-review-2026-08-25.md`、`docs/perf-iteration-a-2026-08-26.md`(原始发现与基线)
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)、`docs/wasm-abi.md`
