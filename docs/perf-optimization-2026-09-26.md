# OpenRusty WASM 性能优化落地报告(2026-09-26)

- 前置阅读: `docs/perf-review-2026-08-25.md`(原始发现, 批次 1/2 清单)与
  `docs/perf-iteration-a-2026-08-26.md`(基线与 paired-delta 方法论)。
- 基线(iteration-A 复测): or-kvprobe(8 阶段插件)约 535 us/req vs 无插件管线
  or-warn 约 74 us/req, 即 wasm 路径放大约 6-8x。
- 本轮对应的只读审查(问题清单): `features/changelog/2026-09-26/wasm-perf-review.md`。

## 0. 摘要

本轮落地审查报告中的 P0/P1a/P2 三项 ABI 边界开销: 加载期
`Linker::instantiate_pre` 预解析导入 + wasmtime pooling allocator(P0),
阶段间 push/pull 改为 `mem::take` 所有权转移(P1a, 深拷贝降为 0), 以及 host 侧
memory 句柄缓存 / `req_meta("body")` 零拷贝 / `kv_get` 单次拷贝 / `resp_header_*`
clone 削减(P2)。P1b(`block_in_place` 过渡卸载)一度实现, 但 paired-delta 实测为
净回归, 已完整回退(见 §3); 消除 worker 阻塞的后续路径是 wasmtime
`async_support(true)` + `call_async`。同时补齐 per-phase 观测: 新直方图
`openrusty_plugin_phase_seconds{phase}` 每请求每阶段恰好记录一次观测,
使后续任何实例化/调用层面的优化可以用数据归因, 而不是靠推断争论——P1b 的
否决正是这套方法论的第一份产出。收益复测沿用 iteration-A 确立的 paired delta
规则(见"验证"节), 最终数字见"实测结果"节: or-kvprobe +~65% rps / −~50%
CPU per req, wasm 边际成本 −~59%。

## 1. P0: 加载期 instantiate_pre + 实例池

- **改动**: 插件加载时用 `Linker::instantiate_pre` 生成预解析计划并随快照发布
  (`LoadedPlugin.pre`); 每请求实例化只是在一个全新 `Store` 上执行该计划,
  不再逐请求做导入解析与导出查找。引擎新增 pooling allocator: 实例从预预留的
  槽位切分, 槽内存按 `plugins.max_memory_mb` 预留(MiB 向上取整到 64 KiB 页,
  上限压在 4 GiB 之下)。
- **位置**: `crates/openrusty-wasm/src/registry.rs`(`new_engine`/`memory_slot_bytes`/
  `LoadedPlugin.pre`)、`crates/openrusty-wasm/src/runner.rs`(从 `InstancePre`
  实例化)、`crates/openrusty-wasm/src/registry_validate.rs`(加载期即跑一次
  `instantiate_pre` 做预检)、`crates/openrusty-core/src/config.rs`(新配置项)。
- **运维可见**:
  - 新配置 `plugins.instance_pool_size`(默认 1000; 0 = 关闭池化, 回到 on-demand
    mmap 分配, 即池化前行为)。
  - 容量按 **插件数 x 峰值并发请求数** 估算; 留量不足时槽耗尽表现为实例化失败,
    走该插件 `plugins.on_failure` 策略(fail_open 放行 / fail_closed 503), 不是
    进程崩溃。
  - 槽位是按 `max_memory_mb` 预留的线性内存上限, 调大内存配额会等比放大池的
    RSS 预留, 二者要一起评估。
- **风险与行为**: 每请求仍是全新 `Store` + 新实例(语义与内存隔离不变), 变化只在
  分配与解析路径; 槽耗尽的新失败模式已收编进 on_failure 语义并计入插件错误
  计数。reload 重建快照时预解析计划随之重建, 无兼容负担。

## 2. P1a: 阶段间状态所有权转移(mem::take)

- **改动**: session 与 HostData 之间的 push/pull 由深拷贝改为 `mem::take`
  所有权双向转移: `ctx`/`peers`/`resp_headers`/`req_body`/`body_chunk` 每次
  插件调用共 0 次深拷贝(此前 8 阶段 x N 插件每请求重复克隆, body_filter 还要
  每 chunk 一次)。
- **位置**: `crates/openrusty-wasm/src/session.rs`(take/take-back 括号不变量:
  两次 take 之间不得 return/continue, 保证数据恒回 session)。
- **运维可见**: 无配置面变化; CPU 收益在带插件路径(kv-probe 类全阶段插件最
  明显)。
- **风险与行为**: 对插件透明(ABI 不变); 实例化失败的插件不参与 take 括号,
  数据不丢。语义不变量由单测锁定。

## 3. P1b: block_in_place 过渡卸载(已实现 -> 实测回归 -> 回退)

- **问题分析(依然成立)**: phase 调用是同步 wasmtime API, 却跑在 tokio 多线程
  runtime 的 worker 线程上; 一次 wasm 调用会占住 worker 直至返回, 插件超时
  上限(`plugins.timeout_ms`)以内的长调用可能饿死同 worker 上的其它任务,
  连无插件请求也会被拖住(审查报告 §2.2 的目标症状)。这个问题真实存在——
  被证伪的只是本轮的解法。
- **实验(已回退)**: 一度实现过渡方案——async handler 内的同步 wasm 调用经
  `tokio::task::block_in_place` 执行, 让当前 worker 先交出 run-queue(仅多线程
  runtime 走该路径, 无 runtime / current-thread 测试运行时回落 inline);
  helper 为 `crates/openrusty-server/src/wasm_offload.rs`, 接线覆盖
  `pipeline/mod.rs`(前置阶段/content/header_filter/log)与
  `pipeline_peer.rs`(balancer)的阶段调用点。
- **实测(paired-delta, 同机同参数与 baseline 成对对比)**: 两条路径同时回归——
  or-warn CPU 80 -> 156 us/req, or-kvprobe rps 11.5k -> 8.6k(中间轮原始数据
  `build/bench/summary-20260926-235627.md`)。
- **根因**: `block_in_place` 每次调用强制把当前任务移出 worker, 做一次
  scheduler run-queue handoff(约 10 us); 卸载点在阶段调用处, 每请求 8 个
  阶段、无插件请求同样按阶段付费(8 x ~10 us 恰与 or-warn +~76 us/req 的
  回退幅度吻合)。对亚毫秒级的插件 phase, 这笔固定开销是量级性放大——防饿死
  的手续费比要防的病更贵。
- **处置**: 完整回退, 不留配置开关。helper `wasm_offload.rs` 删除, 调用点
  恢复普通 `session.run_phase` inline 同步调用; per-phase 指标接线保留
  (观测与卸载无关)。回退后即为最终形态, 复测回到"实测结果"节的数字。
- **后续路径**: wasmtime `async_support(true)` + `call_async` 是消除 worker
  阻塞且不付 per-call handoff 的终态; epoch interruption 机制本身与 async
  兼容, 迁移牵动整个调用链签名(见"未做与后续")。

## 4. P2: ABI 边界开销削减

### 4.1 guest memory 句柄缓存

- **改动**: guest `Memory` 句柄在实例化时缓存进 `HostData`; host imports 每次
  调用直接取缓存句柄, 不再按名 `get_export("memory")` 重查(导出查找仅保留为
  手动实例化 embedder 的兜底)。
- **位置**: `crates/openrusty-wasm/src/runner.rs`(实例化时写入)、
  `crates/openrusty-wasm/src/mem.rs`(`memory_of` 先缓存后兜底)、
  `crates/openrusty-wasm/src/instance.rs`(`HostData` 字段)。
- **运维可见/风险**: 纯内部路径, 无行为差异。

### 4.2 `req_meta("body")` 零拷贝

- **改动**: body payload 改为借用 refcounted `Bytes` 的 `Cow` 视图直写 guest,
  不再每次调用 `to_vec` 重建(此前最多 16 MiB/次, 连 two-phase 的容量探测也
  要付一次); 标量键(method/path/query/version/upstream 等)同样借用。
- **位置**: `crates/openrusty-wasm/src/linker_req.rs`(`req_meta_payload` 返回
  `Cow`)。
- **运维可见/风险**: ABI 与语义不变(guest 侧仍是两段式读取); 单测断言 body/
  method 分支确实借用(指针相等)而非拷贝。

### 4.3 `kv_get` 与 `resp_header_*` clone 削减

- **改动**: `kv_get` 在 DashMap entry 的借用视图内直接把值写进 guest,
  一次拷贝(此前 map -> Vec -> guest 两次); `resp_header_set`/`resp_header_del`
  的字符串 clone 数削减, 语义(大小写不敏感替换/追加、编辑按调用顺序记录)
  不变。
- **位置**: `crates/openrusty-wasm/src/linker_kv.rs`(`kv_get_with` 借用写)、
  `crates/openrusty-wasm/src/linker.rs`(resp_header 路径)。
- **运维可见/风险**: 无; header 编辑记录顺序与替换语义有单测锚定。

## 5. per-phase 观测: `openrusty_plugin_phase_seconds{phase}`

- **改动**: 新 Prometheus 直方图 `openrusty_plugin_phase_seconds{phase}`,
  语义为 **每请求该阶段全部插件调用的累计墙钟时间, 每请求每阶段恰好一次
  观测**(实例化失败也计入该阶段的耗时与调用计数)。桶 25us..1s
  (`PHASE_BUCKETS`, 微秒级粒度, 区别于请求级直方图 5ms..120s)。
- **位置**: `crates/openrusty-server/src/metrics.rs`(`PHASE_BUCKETS`/
  `record_phases`)、`crates/openrusty-server/src/metrics_render.rs`(暴露);
  记录点恰好覆盖三条收尾路径: `pipeline::finish_log`(含短路)、
  `body_filter` 流式收尾(`run_log`, `log_done` 守卫防重)、`ws.rs::run_ws_log`。
- **wasm 侧配套 API**: `RequestSession::take_phase_stats()` 一次性取走累计
  数据(session 侧二次 drain 为空)。
- **运维可见**: `/openrusty/metrics` 新增一个指标族; 直接回答"哪个阶段、哪类
  请求在烧 CPU", 是后续实例池/异步化调优的归因依据。
- **风险与行为**: 观测本身在请求收尾单点记录, 无锁竞争热点(沿用现有 metrics
  单临界区模式)。

## 6. 验证

```sh
# 单元 + 集成 + 插件构建(三层, 与 CI 门禁一致)
cargo test --workspace
bash scripts/build-plugins.sh
bash scripts/integration.sh

# 性能复测: 全矩阵(bare / or-warn / or-info / or-kvprobe / nginx off/on
# x 2 路径 x 3 并发), 产物在 build/bench/results-*.jsonl
bash scripts/bench.sh
```

判读规则沿用 iteration-A 方法论: **跨轮绝对值仅作参考, 结论以同轮内的成对
差值为准**(本轮主判据 or-kvprobe - or-warn 的 wasm 放大倍数是否收窄); 效应
量小于噪声带(约 +/-6 us/req)时必须用同二进制重复运行或交替 A/B 校准。除汇总
延迟外, 对照 `openrusty_plugin_phase_seconds` 的 sum/count 变化可以把收益
归因到具体阶段。

## 7. 实测结果 (2026-09-26, c=100)

- 方法: `scripts/bench.sh` 缩减矩阵(`DURATION=6 WARMUP=1 CONCURRENCY=100
  PATHS=/`), 同机; baseline 为干净 HEAD `53fc7c6` worktree, 跑两轮确认一致
  (or-warn 28.5–29.8k rps / or-kvprobe 11.5–12.0k rps), 结论按 paired-delta
  以同轮成对差值判读。产物: `build/bench/summary-*.md`(optimized 轮为
  `summary-20260927-000337.md`)。
- 校准项: bare 两态均 ~43k rps(loadgen 上限, 持平); nginx-on ~40k rps 持平,
  排除环境漂移。

| 场景 | 指标 | baseline(HEAD 53fc7c6) | optimized | 变化 |
|---|---|---|---|---|
| or-warn(无插件) | rps | 28.5–29.8k | 35.0k | +~20% |
| | p50 | 2.79–2.88 ms | 2.47 ms | −~14% |
| | CPU us/req | ~80 | 63.1 | −~21% |
| or-kvprobe(8 阶段插件) | rps | 11.5–12.0k | 19.4k | **+~65%** |
| | p50 | 7.9–8.4 ms | 4.84 ms | **−~40%** |
| | CPU us/req | 396–421 | 202.5 | **−~50%** |
| wasm 边际成本(kvprobe − warn, CPU/req) | | ~341 us | ~139 us | −~59% |

- 主判据(or-kvprobe − or-warn 的成对差)收敛: wasm 边际成本 341 -> 139
  us/req(−~59%), 边际成本相对无插件管线的放大倍数约 4.3x -> 2.2x。收益主要
  来自 P0(instantiate_pre + 实例池摊薄每请求实例化)与 P1a/P2 的边界拷贝
  消除; `openrusty_plugin_phase_seconds` 各阶段 sum/count 同步下降可作归因
  佐证。P1b 不在其中(已回退, 见 §3)。

### 7.1 集成演练 173/174 的"存量失败"根因(2026-09-27 复盘)

本轮与基线 worktree 都稳定复现的唯一失败 `capped tasks spread over all 3
nodes`(30_plugins_probe.sh 第 23 节)已定位为**环境端口污染, 非代码缺陷**:
宿主机上常驻的 agent 进程以 ~2s 周期轮询 `127.0.0.1:18080/api/nodes/channel`
(默认 GATE_PORT)。第 23 节把 extract 切到 `path:1` 并设 `max_tasks_per_node=1`
后, 这些轮询的 path 首段 `api` 成为 task key, 以 2s 周期不断续期
`aff:api`(TTL 6s, 永远存活), 提前占满一个节点的 cap 槽; 第三个探针任务
pk-c 面对三节点全满只能 Declined 交给默认 SWRR, 扩散断言随之失败。sticky 与
fallback 断言不受影响, 与观察完全一致。换未被轮询的端口复跑
(`GATE_PORT=18099 ./scripts/integration.sh`)得 **174/174 全绿**。演练本机端口
有后台流量时, 请用 `GATE_PORT` 覆盖默认值。

## 8. 未做与后续(future work)

| 项 | 一句话理由 |
|---|---|
| per-request 实例/Store 复用 | 改动面大(需实例 reset 语义), 本轮先吃 instantiate_pre + 池化的确定收益 |
| wasmtime `async_support(true)` + `call_async` | 彻底消除 worker 阻塞的正确终态; `block_in_place` 过渡已实测回归并回退(§3), async 化成为唯一剩余路径, 但牵动整个调用链签名 |
| `orr_alloc` host-push 快路径 | 当前 two-phase 协议配合零拷贝读取已消掉大头, host 主动分配的收益需重新评估 |
| kv_scan 批量编码 | 每条 3 次拷贝仍在, 但使用频率低, 先观察 per-phase 指标再决定 |
| SDK 侧 probe-first 读取 | 会让常见 <=256 B 读取多付一次 host call, 审查判定为净回退 |
| 磁盘编译缓存(`Config::cache`) | 启动/reload 时间优化项, 不在请求热路径 |
| 插件 `opt-level`("s" vs "3")重测 | 实例化成本下降后最优化点可能移动, 待有基线数据再动 |
