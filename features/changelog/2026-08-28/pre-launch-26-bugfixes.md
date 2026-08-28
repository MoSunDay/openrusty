Commit: 4e38037
# 上线前评审：26 项 bug 全量修复（3 高 / 11 中 / 12 低）

## Context
- 五问式上线前评审确认 26 项缺陷，横跨 wasm 运行时语义、host KV 契约、核心 Decision 语义、网关转发与健康、e2e 脚本硬化。本轮一次性修复并逐项配回归测试；语义基准为 nginx 行为与 docs/wasm-abi.md。

## Change Summary
### A. wasm 运行时（crates/openrusty-wasm）
- **epoch 看门狗（高）**：新增 `src/epoch.rs`，engine 级 ticker 线程每 10ms bump epoch；每次调用自带相对 deadline（`runner.rs:155`），修复长调用被他人掐短/永不超时。
- **scan 游标泄漏（高）**：`host_state.rs:38-64` 游标携带 created_at，60s TTL + 每插件 64 个活游标上限，惰性回收。
- **read_guest 越界分配**：`mem.rs:29` 写入前校验 `data_size`。
- **实例化 epoch 超时**：`runner.rs:19/72` `instantiate_with_budget` 给 start 段 5s epoch 预算，恶意模块不再卡死 reload。
- **reload 竞争 CAS**：`registry.rs:44/58` 版本比对 3 次重试后返回 `Conflict`；并发 reload 各发布一代（并发压测覆盖）。
- **插件发现卫生**：`registry_validate.rs:26-43` 非 regular file 跳过并 warn，不再 panic/误加载目录。
- **host KV 配额**：`kv_set` 单值 64 KiB、插件总量 1 MiB 配额，拒绝且不改存储（`host_state.rs`）。

### B. core 语义（crates/openrusty-core）
- **plugins.order 重复项**：`config.rs:237` validate 拒绝重复插件名。
- **client_ip 去 leak**：`context.rs:35-38` 不再 `leak()`，返回 `String`。
- **越界 Deny 防鉴权旁路（高）**：`phase.rs:90/118` `to_abi` 对 `100..=599` 之外的 `Deny` 编码 `-1`（NGX_ERROR），host 按 BadCode 走插件失败策略，而非误判 `Ok`/合法状态码。

### C. sdk + macros 契约
- **kv_scan 两阶段（高）**：`openrusty-sdk/src/host.rs:149` 仅 `-1` 判死游标；`≤-2` 解析为 `-(required_len)` 换大缓冲重试。
- **read 1MiB 封顶**：`ffi.rs:155/185` 超过 `HEAP_SIZE` 干净返回 `None`，不再溢出 arena。
- **#[phase] 签名校验**：`openrusty-macros/src/lib.rs:56/67` 编译期拒绝带参与非 Decision 返回；新增 trybuild UI fixtures 3 组（`openrusty-sdk/tests/ui/`）。

### D. 网关（crates/openrusty-server + openrusty-proxy）
- **max_fails=0 关闭记账**：`health.rs:216/244` nginx 语义（原先反而首次失败即判 down）。
- **重试排除已试节点**：`pipeline.rs:296/323` 失败地址记入 `ReqCtx::tried`，重试与插件 pin 均不再回选。
- **被动健康按地址 remap**：`health.rs:536` reload 后既有状态跟 peer 地址走，新地址从零计。
- **非幂等仅 Connect 重试**：`forward.rs:52` 方法×错误矩阵（nginx 对齐）。
- **WS 头策略复用/XFF 合并**：`ws.rs:34/56` 复用 `merge_xff`，不覆盖既有 X-Forwarded-For。
- **WS 超时 + peer 重试**：`ws.rs:65/100` route timeout 界定每次尝试；无未试 peer 即停。
- **被动健康新鲜 now_ms**：`pipeline.rs:156/277` 记录点现取时刻，不用陈旧时间戳。
- **reload 串行化 + 409**：`state.rs:50` Mutex 串行；`reload.rs:50` try_lock 失败回 409 In-Flight。
- **ClientPool 驱逐**：`client.rs:76/128` 驱逐路径接入 reload/remap（`state.rs:94` 调用）。
- **TokioTimer + 读写超时**：`h2c.rs:15/49` hyper client 池计时器 + idle/读超时，空闲连接回收。
- **探测 tick 并发化**：`active_probe.rs:89` join_all 并发探测，串行 tick 不再拖长周期。
- **metrics 标签钉定**：`pipeline.rs:85/129` reload 前后 route 标签稳定，不漂移。

### E. e2e + 插件
- **integration.sh 硬化**：`scripts/integration.sh:15/48/53` pipefail、require_alive/FATAL、curl `--max-time`，挂死不再假绿。
- **kv-probe 键派生**：`plugins/kv-probe/src/lib.rs:62/274/360` 键按请求派生，消除跨请求串扰假阳性。

## 测试覆盖
| 修复 | 代表测试 | 文件 |
|------|----------|------|
| epoch 看门狗 | `stale_bump_does_not_shorten_the_next_call`、`ticker_advances_the_epoch_until_dropped` | openrusty-wasm/src/{runner,epoch}.rs |
| 游标泄漏/配额 | `error_kinds_sorted_and_kv_len_counts_entries`、TTL/上限边界 | openrusty-wasm/src/host_state.rs |
| 实例化超时 | `instantiation_of_infinite_start_module_times_out` | openrusty-wasm/src/runner.rs |
| reload CAS | `concurrent_reloads_publish_one_generation_each` | openrusty-wasm/src/registry.rs |
| plugins.order 去重 | validate 拒绝重复 | openrusty-core/src/config.rs |
| 越界 Deny | `to_abi`/`from_abi` roundtrip + BadCode 策略 | openrusty-core/src/phase.rs、openrusty-sdk/src/dispatch.rs |
| #[phase] 签名 | trybuild `phase_fail/*` 3 组 | openrusty-sdk/tests/ui/ |
| scan 两阶段/1MiB | `>256B` 重试、超限 `None` | openrusty-sdk/src/{host,ffi}.rs |
| max_fails=0 | 计数/判定守卫 | openrusty-proxy/src/health.rs |
| 已试节点排除 | `pipeline_peer` pick 排除 | openrusty-server/src/pipeline_peer.rs |
| 非幂等重试 | `retry_gate_method_by_error_matrix` | openrusty-proxy/src/forward.rs |
| reload 串行化 | 并发 reload 竞争 e2e | scripts/integration.sh §30 |

- 全量回归：`cargo test --workspace` → **195 passed / 0 failed**（含 trybuild 1）
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 0 警告
- 插件：`scripts/build-plugins.sh` → EXIT=0（kv-probe 27177B / vllm-kv-scheduler 26866B）
- e2e：`scripts/integration.sh` → **102/102 × 3 连跑**（93 原有 + 9 新增），无 flake
- 行数：迭代最大 `host_state.rs` 702 ≤ 800；新增最大 `epoch.rs` 156 ≤ 400

## Impact Surface
- **行为变化**：越界 `Deny` 由误判改为插件失败策略；`max_fails=0` 真正关闭被动记账；非幂等方法仅 Connect 错误重试；并发 reload 返回 409；WS 超时按次尝试计。
- **ABI 契约落盘**：docs/wasm-abi.md 六处（Deny 范围、scan 两阶段、1MiB 封顶、KV 配额、epoch 语义、req_peer_get -2 例外）。
- 不影响：TOML 配置兼容（serde default）、合法插件的既有 ABI 用法、metrics 暴露格式。

## Related Docs
- [docs/wasm-abi.md](../../../docs/wasm-abi.md)
- [agents/wasm-runtime](../../../agents/wasm-runtime/index.md)、[agents/proxy](../../../agents/proxy/index.md)
- [changelog 索引](../../index.md)
