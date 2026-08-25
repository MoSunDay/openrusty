# OpenRusty 性能评审报告（2026-08-25）

- 锚定 commit：`d9281ef`。下文所有 `文件:行号` 均以该提交为准，行号会随后续提交漂移。
- 本报告只评审、不改代码。每项统一格式：**现状 → 影响 → 建议改法 → 风险/兼容性 → 预期收益量级**。
- 除特别标注"实测"外，收益量级均为代码结构推断；建立压测基线（见 §4）是所有深度项的前置条件。

## 0. 背景与结论

### 0.1 关于「开启 aio」

nginx 的 `aio` 指令（`aio threads` / POSIX AIO）解决的是**静态文件磁盘 IO 不阻塞 worker** 的问题。
本网关没有静态文件服务，不存在可"开启"的 aio 开关。网络侧 Tokio 已是 epoll 事件驱动
（等价于 nginx 的 event 模型），按既定决策不引入 io_uring/tokio-uring。

但存在一个真正等价于"缺 aio"的问题，且是当前最大的性能/稳定性风险：

> **WASM phase 是同步调用，直接在 Tokio worker 线程上执行**（`crates/openrusty-wasm/src/runner.rs:138`，
> 调用链 `crates/openrusty-wasm/src/session.rs:135-136`、`crates/openrusty-server/src/body_filter.rs:98`，
> 甚至 `Drop` 里也跑 wasm，`body_filter.rs:143-162`）。
> 这等于把 nginx 里"磁盘 IO 阻塞 worker"换成了"wasm 执行阻塞 worker"。详见 §2.2。

### 0.2 总体判断

- **无插件时**：热路径基本是 axum/hyper 默认栈 + 管线胶水代码，接近裸 hyper 水平；
  可挖的空间集中在 metrics 全局锁、日志同步写、连接回收等系统级问题（§1）。
- **带插件时**：wasm 路径是数量级级热点——每请求每插件完整实例化（§2.1）+
  同步阻塞 worker（§2.2）+ 每 phase 双向深拷贝（§2.3）。短请求（echo 场景）下，
  这些开销大概率超过转发本身的 CPU 占比（推断，待基线验证）。

---

## 1. 批次 1 · 快速项（低风险，建议先做）

### 1.1 访问日志同步写 stdout，无 non_blocking

- **现状**：`crates/openrusty-server/src/main.rs:54-56` 用 `tracing_subscriber::fmt().init()`
  初始化——默认 writer 是 stdout，阻塞写；每请求访问日志在
  `crates/openrusty-server/src/body_filter.rs:66-77`（`run_log` 里的 `tracing::info!("request finished")`）
  同步格式化并写出。
- **影响**：热路径上每请求至少一次字段格式化 + 一次 `write(2)`；stdout 管道消费慢
  （如 journald 抖动）时直接阻塞 Tokio worker，尾部延迟被日志系统绑架。
- **建议改法**：`tracing_appender::non_blocking`（bounded 通道 + 后台写线程）包 stdout writer；
  或将访问日志降为可采样/可关（`log_level` 已有配置面）。
- **风险/兼容性**：bounded 通道满或进程崩溃时丢尾部日志（有界丢失，需接受）；
  `integration.sh` 若断言日志行需回归。
- **预期收益量级**：消除每请求 1 次 syscall + 日志抖动传导到 p99；吞吐小升（~几%，推断）。

### 1.2 metrics 全局 `Mutex<MetricsState>` + 标签 String 分配

- **现状**：`crates/openrusty-server/src/metrics.rs:76-78`，单一 `Mutex<MetricsState>`；
  每请求 `crates/openrusty-server/src/app.rs:51-52` 调 `record_request` + `record_duration`
  （≥2 次全局锁/请求），每次 upstream attempt 再加一次 `record_attempt`（`pipeline.rs:240` 等）；
  `metrics.rs:91` 的 key `(route.to_string(), code)` 每次计数都重新分配 label String。
- **影响**：多 worker 并发下全局锁成为串行点（锁持有期内还做 HashMap entry 分配）；
  label String 分配随 QPS 线性增长。
- **建议改法**：分片计数（如按 worker 数 N 个分片，scrape 时合并）；label 换
  `(route_id: u32, code)` 或预 intern 的 `&'static str`；或直接换 `metrics` crate 的无锁实现。
- **风险/兼容性**：`/openrusty/metrics` 输出必须保持 Prometheus 0.0.04 文本格式与现有指标名
  （集成测试有断言）。
- **预期收益量级**：消除全局串行点，高并发下吞吐提升明显（worker 越多越显著，推断）。

### 1.3 hyper client 池未设 pool_timer，idle_timeout 实际不生效

- **现状**：`crates/openrusty-proxy/src/client.rs:58-64`，`Client::builder(...).build(connector)`
  未调用 `.pool_timer(TokioTimer::new())`。hyper-util 0.1.20 的 legacy client 只有提供了
  timer 才会 spawn 空闲连接驱逐任务（`Pool::spawn_idle_interval` 依赖 `timer: Option<M>`）；
  无 timer 时默认的 90s `idle_timeout` 不会触发。
- **影响**：每个用过的 upstream 地址的 keep-alive 连接永远不回收，上游摘除/发布后
  网关侧残留半死连接；长时间运行 fd 单调增长，放大 §1.5 的 fd 压力。
- **建议改法**：`build()` 里加 `.pool_timer(TokioTimer::new()).pool_idle_timeout(Duration::from_secs(60))`
  （时长进配置）。
- **风险/兼容性**：无行为风险；idle_timeout 过短会降低 keep-alive 命中率，取 60~90s 为宜。
- **预期收益量级**：资源正确性问题（fd 不再泄漏），吞吐无变化。

### 1.4 响应 body 与 WS 握手无超时

- **现状**：`crates/openrusty-server/src/pipeline.rs:232-236`，`tokio::time::timeout` 只包住
  `proxy::forward`——它在拿到**响应头**后即返回，`Incoming` body 随后在
  `FilteredBody::poll_frame`（`body_filter.rs:88-140`）里流式转发，全程无超时；
  `crates/openrusty-server/src/ws.rs:96` 的 WS 握手 `client.request(outbound).await` 是裸 await
  （`connect_timeout` 只覆盖 TCP 建连）。
- **影响**：慢上游以 1 byte/s trickle body 或握手挂起时，客户端连接 + `RequestSession`
  （含全部插件实例内存）被无限期占用；`retry_on_timeout` 对该阶段完全失效；
  构成慢速 DoS 面。
- **建议改法**：body 侧加**chunk 间 idle 超时**（不能是 overall——SSE/大文件会被误杀），
  如包一层 `tokio::time::timeout` 在每次 poll 读上；WS 握手包 route timeout。
- **风险/兼容性**：idle 超时阈值需可配且默认宽松；流式上游（SSE）需回归测试。
- **预期收益量级**：稳定性/资源占用上限问题，消除无界占用。

### 1.5 systemd 单元无 `LimitNOFILE`

- **现状**：`scripts/install-service.sh:48-81` 生成的 echo 模板单元与 gateway 单元均未设置
  `LimitNOFILE`，落回 systemd/发行版默认（常见 soft 1024）。
- **影响**：每并发连接至少占 2 个 fd（客户端侧 + 上游 keep-alive 侧），~500 并发即触顶，
  表现为 accept 失败/上游建连失败，且很难从日志定位。
- **建议改法**：gateway 单元 `[Service]` 段加 `LimitNOFILE=65535`（echo 上游可不加）。
- **风险/兼容性**：无。
- **预期收益量级**：消除高并发部署的硬天花板，属于部署正确性修正。

---

## 2. 批次 2 · 深度项（结构性改动，收益最大）

### 2.1 wasm 每请求每插件完整实例化，无 PoolingAllocator

- **现状**：`crates/openrusty-wasm/src/runner.rs:42-70` 的 `instantiate()` 每次
  `Store::new` + `linker.instantiate`；`crates/openrusty-wasm/src/session.rs:22` 的
  `rts: Vec<Option<PluginRt>>` 随每请求 `RequestSession::new`（`pipeline.rs:147`）创建，
  首个 phase 触发惰性实例化（`session.rs:108-124`）；Engine 未配置
  PoolingAllocator（`registry.rs:52-56`，默认 on-demand 分配策略）。
  （这也是 `docs/wasm-abi.md` 文档化的现行设计。）
- **影响**：每请求每插件一次完整实例化：内存创建、导入解析、导出函数查找。
  实例化成本通常在几十 µs～ms 级（取决于模块大小），对 echo 类短请求是
  CPU 大头（推断——**当前最大单项热点候选**）。
- **建议改法**：
  1. `Linker::instantiate_pre` 预链接得到 `InstancePre`（重载时构建一次，随快照发布），
     每请求 `InstancePre::instantiate`，跳过导入解析；Store data（HostData）本就是
     实例化时传入，与 InstancePre 复用不冲突。
  2. `wasmtime::Config::allocation_strategy(PoolingAllocator)` + 预热，把每实例的
     内存分配从 mmap 降为 arena 切分。
  3. 进一步（可选）：实例池化复用 + per-request reset，改动面大，先做 1+2。
- **风险/兼容性**：PoolingAllocator 需要预估实例数/内存页上限（配错会实例化失败）；
  `docs/wasm-abi.md` 需同步更新"once per request"的描述。
- **预期收益量级**：带插件路径的数量级提升（推断；wasmtime 官方基准支持
  instantiate_pre + pooling 的量级判断）。

### 2.2 wasm 同步调用阻塞 Tokio worker

- **现状**：`runner.rs:138` `rt.on_phase.call(&mut rt.store, ...)` 是同步调用。
  调用链：前置 phase 在 `session.rs:135-136`（请求 handler 的 async 上下文里直接调）；
  `body_filter` 在 `body_filter.rs:98` 的 `poll_frame` 内调（还持有 session mutex）；
  `Drop for FilteredBody`（`body_filter.rs:143-162`）在客户端中途断开时于 Drop 里跑 wasm。
- **影响**：wasm 执行期间（上限 = plugin timeout，watchdog 才会 trap）Tokio worker
  线程被占死。默认 worker 数 = CPU 核数，N 个并发慢插件即占满 N 个 worker，
  整个网关（含无插件请求、管理端点）停摆；Drop 里跑 wasm 还发生在 executor 线程的
  同步段，完全无法让出。这是 §0.1 说的"真正缺 aio"问题。
- **建议改法（两方案对比）**：
  - **方案 A（过渡）**：`tokio::task::spawn_blocking` 包 `runner::run_phase`。
    改动小；`Store` 是 `Send` 可移入闭包。代价：每 phase×每插件一次 blocking 池
    往返，插件很快（µs 级）时开销可能反噬吞吐；默认 blocking 池 512 线程，
    需评估极端并发下的线程数。
  - **方案 B（长期，推荐）**：`wasmtime::Config::async_support(true)` +
    `func.call_async`。wasm 内的 loop 回边会 yield，彻底不占 worker；
    代价是 linker 所有 host fn 改 async 签名（ABI 行为不变，纯 host 侧改动），
    并需配 `async_set_stack_size`。epoch watchdog 机制保留。
  - `Drop` 路径两个方案下都需重构：客户端断开时把"收尾 wasm 调用"转交后台任务
    （如 `tokio::spawn` 拿走 session），而不是在 Drop 里同步跑。
- **风险/兼容性**：方案 B 触及 `openrusty-wasm` 全部 host imports（`linker*.rs`）与
  runner；方案 A 需把前置 phase 的 session 从栈所有权改成共享（body_filter 已经是
  `Arc<Mutex<RequestSession>>`，前置 phase 也要对齐）。Decision 语义不变。
- **预期收益量级**：慢插件场景下从"整机停摆"变为只影响该请求；正常插件吞吐
  变化待测（A 可能小降，B 持平或小升）。

### 2.3 每 phase × 每插件两轮深拷贝 ctx/peers/resp_headers

- **现状**：`session.rs:126-141`，调用前 push：`hd.ctx = self.ctx.clone()`、
  `hd.peers = self.peers.clone()`、`hd.resp_headers = self.resp_headers.clone()`
  （128-130 行；`req_body`/`body_chunk` 是 `Bytes`，clone 仅引用计数，代价小）；
  调用后 pull：`self.ctx = hd.ctx.clone()`、`self.resp_headers = hd.resp_headers.clone()`
  （140-141 行）。`ReqCtx` 含 `method/path/query: String` + `headers: Vec<(String,String)>`
  （`crates/openrusty-core/src/context.rs:11-18`），clone 是全量深拷贝。
- **影响**：成本 = O(headers×2) String 分配 × 每插件 × 每 phase（8 个 phase）。
  kv-probe 全阶段插件 + 30 个请求头的场景 ≈ 每请求数百次 String 分配（推断）。
- **建议改法**：host 侧 mutation 标记（`resp_header_set/del`、`ctx` setter 时置 dirty），
  pull 侧仅 dirty 时拷回；push 侧 `peers` 仅 balancer phase 需要全量，其余 phase
  可传空/共享；或 `Arc<ReqCtx>` + COW。
- **风险/兼容性**：dirty 跟踪必须覆盖所有会改 `hd.ctx`/`hd.resp_headers` 的 host
  imports，漏标 = 插件改写静默丢失（行为回归，需 e2e 全量跑）。
- **预期收益量级**：带插件路径分配次数显著下降（推断，中到大，取决于头数×插件数）。

### 2.4 watchdog 每 phase spawn 不可取消 task + epoch 全局串扰

- **现状**：`runner.rs:132-137` 每次 `run_phase_outcome` 前 `set_epoch_deadline(1)` +
  `arm_watchdog`；`runner.rs:176-185` watchdog `spawn` 一个 `sleep(timeout)` 后
  `engine.increment_epoch()` 的 task，不可取消；epoch 是 **Engine 级全局**
  （`registry.rs:52-56` 单一 Engine 服务所有插件×所有请求）。
- **影响**：
  1. 每 phase×每插件一个 timer task，正常快调用也留一个空转到期的 task；
     8 phase × N 插件 × C 并发 = 大量短命 task 分配；
  2. 任一请求超时的 bump 会推进**所有**正在执行的 wasm 调用的 epoch；配合
     rebase-to-1 语义，一个 stale bump 可能提前 trap 无辜调用（`runner.rs:132-135`
     注释已承认此 collateral）。
- **建议改法**：改为 wasmtime 推荐的 engine-wide ticker 模式——启动一个常驻 task
  每 tick（如 10ms）`increment_epoch`，调用前
  `set_epoch_deadline((timeout / tick).ceil())`。per-call watchdog 与串扰同时消除。
- **风险/兼容性**：超时精度降为 tick 粒度（可接受）；常驻 ticker 每 10ms 一次
  原子写，开销可忽略。
- **预期收益量级**：消除 per-call task 分配；更重要的是消掉跨请求误 trap 的
  正确性隐患。

---

## 3. 次要清单（记录，不展开）

| 项 | 位置 | 一句话 |
|---|---|---|
| body 缓冲 16MiB 硬编码不可配 | `pipeline.rs:19-20` | 应进 route/server 配置；超限行为可配 |
| 无并发连接上限 | `crates/openrusty-server/src/h2c.rs` | 无 semaphore/连接数限制，过载时靠 fd 耗尽兜底 |
| health 两把全局 Mutex | `health.rs:63,67` | 每请求 pick+record 多次过全局锁，可 `RwLock`/分片/sharded map |
| 每 attempt 重复 parse header | `forward.rs:111-125` | `HeaderName/Value::from_bytes` × 头数 × attempt，可在首次解析后缓存 |
| fallback 二次路由 | `app.rs:41-50` | `handle_request` 已 match 过 route，出路径再 match 一次只为 metrics label |
| `req_meta("body")` 全量 `to_vec` | `linker_req.rs:84` | 每次调用整份 body 拷给 guest，可加长度上限或 offset 分段 |
| 无 worker_threads 配置面 | `crates/openrusty-core` ServerConfig | tokio 默认 = CPU 核数，无法按部署调优 |
| release profile 仅 thin-LTO | 根 `Cargo.toml [profile.release]` | 可试 `lto = "fat"` + `codegen-units = 1`；`panic = "abort"` 需评估（无 catch_unwind，但 abort 会放大任何 host bug） |

---

## 4. 建议的验证基线（本次未实施，列为第一建议）

任何 §2 改动前先落一个 `scripts/bench.sh`（wrk 或 hey），固定三场景：

1. **裸上游基线**：直连 echo upstream（`examples/echo_upstream`）；
2. **无插件网关**：经 openrusty 转发，插件目录为空；
3. **带插件网关**：`kv-probe`（全 8 phase）+ 可选 `vllm-kv-scheduler`。

记录 p50/p90/p99、RPS、网关进程 CPU%、RSS；2/1 与 3/2 的差值即"管线开销"与
"wasm 开销"的归因基线。§2.1/§2.2/§2.3 每项落地后复测对比，避免无数据争论。

场景建议：`-c 50/-c 500/-c 2000` 三档并发 × 60s；上游延迟注入（echo 加 sleep）
用于验证 §2.2 的慢插件场景不再拖垮无插件请求。
