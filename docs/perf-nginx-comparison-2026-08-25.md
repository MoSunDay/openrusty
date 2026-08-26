# OpenRusty × nginx 对比压测报告（第二轮 · 2026-08-25）

- 网关代码锚定 `81e347b`（第一轮评审报告 `perf-review-2026-08-25.md` 之后零网关代码改动）；
  本轮新增 `scripts/bench_loadgen.py` + `scripts/bench.sh`，数据由它们实跑取得。
- 结论以 **µs/req（进程侧 CPU/请求）** 为主判据；RPS/延迟受单进程压测器天花板影响，只作横向对比（§1.4）。

## 0. TL;DR

| 结论 | 数字 |
|---|---|
| 无插件管线 vs nginx（同路径公平对齐） | **69 vs 11.5 µs/req ≈ 6.0×**；RPS 为 nginx 的 74–76% |
| 访问日志成本（info 开→关） | **+29.9 µs/req（+43%）**，RPS −13%；nginx 同项 **+1.75 µs**（17×差距） |
| kv-probe（全 8 phase wasm） | **+499 µs/req（7.2×）**，p99 @c=2000 达 **857–917ms**（同步 wasm 饱和） |
| 每请求头边际成本（+20 头实验） | 网关 **2.9 µs/头** vs nginx 0.05 vs 裸 echo 0.21（**58×**） |
| 网关单跳延迟（p50, c=100） | **+0.34–0.48ms** vs nginx ≈ +0；syscall 实验排除包化差异（N10） |
| 内存 | 网关 20→92MB（随 c）；kv-probe 202MB；nginx 153–172MB/16 worker |

差距归因链（§3）：69 µs ≈ 头部物化 10–15 + metrics 3–6 + 选路/健康 4–8 + 元数据字符串 3–6 + 时钟 1–2 + 其余（axum/tokio 胶水，待火焰图）25–35。
行动顺序更新见 §5：迭代 A 吸收 **N2/N3/N8**。

## 1. 方法论

### 1.1 场景与矩阵

6 场景 × 2 路径（`/` 小响应 ~13B；`/echo` 头回显 JSON ~400B）× c=100/500/2000 × 10s（预热 2s/场景×路径）：

- **bare**：直连 3 个 echo upstream（127.0.0.1:9001-3，systemd 单元），worker 按下标轮询三地址；
- **or-warn / or-info**：`openrusty`（私有实例 :18181，私有配置/日志），log_level=warn/info，插件目录为空；
- **or-kvprobe**：同 or-warn + 仅 `kv-probe.wasm`（全 8 phase，order=["kv-probe"]）；
- **nginx-off / nginx-on**：系统 nginx 二进制起的**私有实例**（:18182，独立 pid/conf/log/temp，系统 :80 实例不受影响），access_log off/on。

上游均为本机 echo（`examples/echo_upstream`，axum/hyper），3 peer SWRR。

### 1.2 nginx 公平性对齐

为对齐网关的上游侧行为（`proxy/forward.rs`），nginx 配置固定：`proxy_http_version 1.1` +
`proxy_set_header Connection ""` + upstream `keepalive 256` + `proxy_set_header Host $upstream_addr` +
`X-Forwarded-For $proxy_add_x_forwarded_for` + `proxy_buffering off`；`worker_processes auto`（16 核）。
nginx 其余保持默认（含 `keepalive_requests 100`：每连接第 100 个响应带 `Connection: close`，
压测器按语义静默重连、不计错误——网关侧 hyper 无此限制，属真实行为差异，如实保留）。

### 1.3 测量口径

- **压测器** `scripts/bench_loadgen.py`：stdlib asyncio，每 worker 1 条持久 HTTP/1.1 连接，
  客户端 socket 设 TCP_NODELAY；延迟覆盖写请求→读完响应体；非 200/超时/解析失败计 err。
- **CPU%**：`/proc/<pid>/stat` utime+stime 差值（CLK_TCK=100）除以墙钟；nginx 为 master+16 worker 求和；
  **µs/req** = CPU 秒差 × 1e6 / 请求数（bare 场景计的是 echo 进程组，代理场景只计代理自身）。
- **VmHWM**：场景内累计峰值（跨路径/并发单元格单调不减）；nginx 为多进程求和（共享页重复计入）；
  bare 为 systemd 单元自 8/23 起的 lifetime 峰值，仅作参考。
- **err**：全矩阵 36 单元格 err=0。

### 1.4 局限与风险披露

1. **loadgen 单进程天花板 ~44k RPS**：bare 在 c=100/500 到 44.8k/43.6k 而 echo CPU 仅 40–73%，
   c=2000 反降至 35.1k——瓶颈在压测器。因此 RPS 比值只取"远低于天花板"的场景对比；
   **µs/req 不受影响**（进程侧测量）。
2. **闭合环路 p50 伪影**：压测器饱和时 p50 ≈ c/RPS（Little 定律），延迟绝对值无意义，
   仅同 c 跨场景可比。
3. **同机抢核**：压测器/网关/nginx/echo 共 16 核，代理场景的绝对 RPS 保守；µs/req 仍可比。
4. **echo 无 TCP_NODELAY（server 侧实测未设）**：本轮未出现 40ms 台阶（请求/响应均单段，
   客户端已 nodelay；syscall 实验 reads/req=1.00 佐证），多段响应场景仍有风险（N12）。
5. 单次采样无重复；侧实验（§2.2）中 or-warn 复跑两轮 66.4/67.0 µs/req，波动 ~1%。

## 2. 数据

### 2.1 全矩阵（36 格，`build/bench/summary-*.md` 由脚本再生）

| scenario | path | c | rps | p50 | p90 | p99 | err | cpu% | µs/req | hwm MB |
|---|---|---|---|---|---|---|---|---|---|---|
| bare | / | 100 | 44826.7 | 2.18 | 2.415 | 2.887 | 0 | 40.4 | 9.0 | 24.2 |
| bare | / | 500 | 43560.8 | 11.175 | 12.451 | 14.838 | 0 | 43.5 | 10.0 | 33.5 |
| bare | / | 2000 | 35145.5 | 56.143 | 60.199 | 75.341 | 0 | 38.6 | 11.0 | 85.8 |
| bare | /echo | 100 | 43261.7 | 2.115 | 2.415 | 7.972 | 0 | 73.2 | 16.9 | 85.8 |
| bare | /echo | 500 | 43579.8 | 11.321 | 11.939 | 13.702 | 0 | 59.4 | 13.6 | 85.8 |
| bare | /echo | 2000 | 33135.4 | 57.381 | 61.945 | 161.427 | 0 | 53.3 | 16.1 | 119.6 |
| or-warn | / | 100 | 31870.5 | 2.615 | 5.11 | 7.508 | 0 | 214.2 | 67.2 | 20.2 |
| or-warn | / | 500 | 34333.7 | 13.373 | 19.06 | 26.883 | 0 | 229.2 | 66.8 | 30.9 |
| or-warn | / | 2000 | 30249.7 | 63.767 | 74.673 | 84.229 | 0 | 213.6 | 70.6 | 78.8 |
| or-warn | /echo | 100 | 32112.6 | 2.575 | 4.967 | 7.465 | 0 | 227.1 | 70.7 | 78.8 |
| or-warn | /echo | 500 | 31704.7 | 14.363 | 21.542 | 29.294 | 0 | 223.7 | 70.5 | 78.8 |
| or-warn | /echo | 2000 | 29695.5 | 64.28 | 79.52 | 91.592 | 0 | 224.7 | 75.7 | 92.1 |
| or-info | / | 100 | 27317.9 | 3.078 | 6.099 | 8.077 | 0 | 263.1 | 96.3 | 18.4 |
| or-info | / | 500 | 27664.7 | 16.788 | 24.304 | 31.935 | 0 | 265.3 | 95.9 | 29.1 |
| or-info | / | 2000 | 26847.0 | 72.116 | 88.875 | 103.273 | 0 | 271.8 | 101.2 | 66.4 |
| or-info | /echo | 100 | 25914.2 | 3.302 | 6.433 | 8.193 | 0 | 262.7 | 101.4 | 66.4 |
| or-info | /echo | 500 | 26420.1 | 17.758 | 25.064 | 30.902 | 0 | 262.5 | 99.4 | 66.4 |
| or-info | /echo | 2000 | 26008.2 | 75.203 | 90.11 | 101.875 | 0 | 269.8 | 103.7 | 75.7 |
| or-kvprobe | / | 100 | 10522.2 | 8.406 | 16.864 | 26.592 | 0 | 616.0 | 585.4 | 34.6 |
| or-kvprobe | / | 500 | 8579.4 | 48.51 | 112.962 | 170.315 | 0 | 603.3 | 703.2 | 67.9 |
| or-kvprobe | / | 2000 | 8283.1 | 61.657 | 501.468 | 916.71 | 0 | 607.2 | 733.1 | 187.2 |
| or-kvprobe | /echo | 100 | 11001.6 | 7.93 | 16.035 | 26.523 | 0 | 605.7 | 550.6 | 187.2 |
| or-kvprobe | /echo | 500 | 10070.4 | 40.644 | 99.06 | 156.387 | 0 | 607.6 | 603.4 | 187.2 |
| or-kvprobe | /echo | 2000 | 9545.1 | 64.56 | 470.072 | 856.047 | 0 | 605.7 | 634.6 | 202.4 |
| nginx-off | / | 100 | 43072.0 | 2.136 | 2.428 | 4.851 | 0 | 51.2 | 11.9 | 156.0 |
| nginx-off | / | 500 | 41076.6 | 11.538 | 12.732 | 17.76 | 0 | 46.0 | 11.2 | 158.1 |
| nginx-off | / | 2000 | 35828.5 | 54.652 | 56.134 | 63.981 | 0 | 43.9 | 12.3 | 164.8 |
| nginx-off | /echo | 100 | 42440.5 | 2.24 | 2.479 | 3.434 | 0 | 47.3 | 11.1 | 164.8 |
| nginx-off | /echo | 500 | 40000.9 | 11.686 | 13.432 | 20.056 | 0 | 48.9 | 12.2 | 164.8 |
| nginx-off | /echo | 2000 | 33601.2 | 58.217 | 60.396 | 70.055 | 0 | 43.6 | 13.0 | 166.4 |
| nginx-on | / | 100 | 45050.4 | 2.074 | 2.287 | 3.309 | 0 | 58.5 | 13.0 | 153.3 |
| nginx-on | / | 500 | 41775.0 | 11.479 | 12.063 | 14.578 | 0 | 54.7 | 13.1 | 155.9 |
| nginx-on | / | 2000 | 33686.8 | 58.197 | 60.443 | 74.013 | 0 | 51.5 | 15.3 | 170.1 |
| nginx-on | /echo | 100 | 42933.4 | 2.217 | 2.44 | 3.634 | 0 | 58.1 | 13.5 | 170.1 |
| nginx-on | /echo | 500 | 40809.8 | 11.704 | 12.429 | 16.17 | 0 | 56.9 | 13.9 | 170.1 |
| nginx-on | /echo | 2000 | 30225.3 | 60.08 | 73.38 | 170.287 | 0 | 69.3 | 22.9 | 172.0 |

### 2.2 侧实验

**头部成本**（`/` 固定小响应，c=100×5s，`--extra-headers 20`，µs/req）：

| 目标 | 0 头 | +20 头 | Δ/20 头 | Δ/头 |
|---|---|---|---|---|
| bare echo | 8.5 | 12.8 | +4.3 | 0.21 µs |
| or-warn | 66.4 | 124.2 | **+57.8** | **2.9 µs** |
| nginx-off | 10.9 | 11.9 | +1.0 | 0.05 µs |

（or-warn 加 20 头后 RPS 34.1k→23.6k，−31%。）

**syscall/包化**（strace -c，c=32×3s）：bare / or-warn / nginx-off 均 **1.00 recvfrom + 1.00 sendto / req**
——响应都是单段单次可读，包化不是差距来源；strace 下 or-warn/nginx RPS 比 11.3k/12.8k≈0.88，
与无 strace 的 0.74–0.76 同向（strace 压低绝对值，比值仍稳定偏低）。

## 3. 差距归因链

以 c=100 双路径均值分解（µs/req，网关进程侧）：

| 层 | µs/req | 依据 |
|---|---|---|
| nginx 代理（对齐后） | 11.5 | 实测 |
| or-warn 无插件管线 | 69.0 | 实测（67.2/70.7） |
| **Δ = 管线差距** | **57.5（6.0×）** | |
| ├ 头部物化往返（N1） | 10–15 | 2.9µs/头 × 默认 ~5 头外推（保守） |
| ├ metrics 双锁+二次路由（N2） | 3–6 | 结构推断（无独立开关可测） |
| ├ 选路/健康分配+锁（N5） | 4–8 | 结构推断 + hdr 实验第二轮疑似健康翻动放大 |
| ├ 元数据字符串（N9） | 3–6 | route.clone/XFF join/Host 等逐项 |
| ├ now_ms 时钟 ×3+（N6） | 1–2 | vDSO 调用次数 |
| └ 其余（axum Router/fallback、ReqCtx/session 构建、FilteredBody 包装、tokio 唤醒链） | 25–35 | 差额；N10 待火焰图 |
| or-info − or-warn = 访问日志（N3） | **+29.9** | 实测（96.3/101.4 vs 67.2/70.7） |
| nginx-on − nginx-off = nginx 日志 | +1.75 | 实测（17× 差距：fmt+stdout vs 预排格式） |
| or-kvprobe − or-warn = wasm 8 phase | **+499** | 实测（550–733 vs 67–76） |

wasm +499µs 与第一轮 §2.1/§2.3/§2.4 一致：每请求 8 phase ×（完整实例化 `runner.rs:42-70` +
双向深拷贝 `session.rs` + watchdog spawn `runner.rs:176`）；c=2000 时同步 wasm 把 16 worker 中
~6 核打满（cpu 605–616%），队列堆出 p99 857–917ms——第一轮 §2.2"慢插件拖垮全局"的定量佐证。

## 4. 新点 N1–N12（五段化；不重复第一轮编号项，除标注"补实测"者）

### N1 头部字符串物化全链
- **现状**：入站 `HeaderMap → Vec<(String,String)>` 每名/值各一次 `to_string`（`pipeline.rs:100-108`）→
  `ReqCtx` 深拷贝（`:125`）→ 每 attempt 再 clone（`:228`）→ `HeaderName::from_bytes` 重新解析
  （`proxy/forward.rs:111-125`）+ `is_hop_by_hop` 线性扫描（`proxy/upstream.rs`）；响应侧
  `HeaderMap → Vec` （`pipeline.rs:290-300`）→ `to_vec()` 再 clone（`:306`）→ 再解析回 `HeaderMap`（`:318-324`）。
- **影响**：实测每请求头边际成本 2.9µs（nginx 0.05、裸 hyper 0.21）；20 头即 −31% RPS。
  头数多的真实 API（cookie/trace/租户头 10–30 个）下这是最大单项。
- **建议改法**：入站保 `HeaderMap` 传递；`ForwardRequest` 携带引用/`HeaderMap` 直接构建；
  字符串物化只在 wasm ABI 边界按需发生（或 ABI 改字节切片 + dirty 标记，与 §2.3 合并设计）。
- **风险/兼容性**：ABI 演进需版本协商；WS 路径（`ws.rs:51-88`）与 hop-by-hop 过滤同步改造。
- **预期收益量级**：20 头场景 −57.8µs/req（−46%）；默认头数 −10–15µs/req（中到大）。

### N2 metrics 双锁双分配 + 二次路由
- **现状**：`record_request`/`record_duration` 两次全局 Mutex（`metrics.rs:89-113`，`app.rs:51-52`）；
  每次 attempt 再一次（`pipeline.rs:216,240,252,271`）；`req.uri().path().to_string()` +
  `match_route` 全表重扫 + `path_prefix.clone()`（`app.rs:41-50`）；attempts 记录再 2 次 `to_string`。
- **影响**：每请求 3–4 次全局锁 + 3–4 次分配 + 一次多余路由匹配（第一轮 §1.2 的补全项）。
- **建议改法**：一次临界区合并全部计数；路由 label 由 `handle_request` 已匹配结果传入（消二次 match）；
  label 用 `&str`/u32 intern。
- **风险/兼容性**：`/openrusty/metrics` 渲染取快照路径（`metrics.rs:131`）需同步调整；无外部契约变化。
- **预期收益量级**：−3–6µs/req + 高并发下锁竞争尾延迟下降（小到中，且是迭代 A 最便宜的锁消除）。

### N3 访问日志 29.9µs/req（第一轮 §1.1 的实测补全）
- **现状**：info 级访问日志（`pipeline.rs:25-30` finish_log / `body_filter.rs:67-78` run_log）经
  `tracing_subscriber::fmt` 同步写 stdout；生产默认 `log_level = "info"`。
- **影响**：实测 +29.9µs/req（+43% CPU）、−13% RPS；nginx 同项 +1.75µs（差 17×，fmt 字段格式化 + stdout 管道写）。
- **建议改法**：生产默认改 `warn`；`non_blocking` writer；流式路径字段固定布局（status/peer/bytes/ms 已是），
  `finish_log` 的 `path.clone()`（`pipeline.rs:27`）去掉。
- **风险/兼容性**：日志顺序在 non_blocking 下弱有序；运维采集需确认。
- **预期收益量级**：+18% RPS（27k→32k 量级），−30µs/req（大，单项第一）。

### N4 client 池全局 Mutex 每请求过锁 + connect timeout 首创定格
- **现状**：`ClientPool { Mutex<HashMap<SocketAddr, HttpClient>> }` 每请求 `get` 过锁
  （`proxy/client.rs:25,47-55`）；某 peer 的 `connect_timeout` 在首次创建时定格（`:44-64`）。
- **影响**：每请求一次全局锁；reload 后无法调整超时语义（第一轮 §1.3 只覆盖了 pool_timer 缺失）。
- **建议改法**：配置期预建 per-peer client 存入 `Arc`（reload 重建整表），请求路径无锁查表；
  与 §1.3 `pool_timer` 一并落。
- **风险/兼容性**：peer 动态增减只经 reload，语义不变。
- **预期收益量级**：−1–2µs/req + 消除一个全局锁（小）。

### N5 选路预算与健康注册表
- **现状**：SWRR 每次 attempt 分配 `weights`/`subset` 两个 Vec（`pipeline_peer.rs:67-81`）；
  `healthy_indices` 每次 attempt 分配 Vec + 过注册表锁（`health.rs:109-149`）；每请求构建
  `PeerView` 3 peer × 2 次 `to_string`（`pipeline.rs:134-144`）+ 注册表锁两次。
- **影响**：每请求 ~6 次分配 + 3 次全局锁；侧实验中 or-warn 第二轮 34k→24k、119µs/req 的疑似
  健康翻动放大（backlog 抖动 → connect fail → max_fails 翻动 → 重试放大）未定论但方向可疑。
- **建议改法**：per-upstream 健康快照 `Arc<Vec<PeerHealth>>`（写时重建）；SWRR 改无分配轮转；
  PeerView 预格式化缓存于 peer 结构。
- **风险/兼容性**：健康状态可见性从强一致退化为快照粒度（毫秒级，可接受）。
- **预期收益量级**：−4–8µs/req；重试放大链路消除后尾延迟更稳（中）。

### N6 now_ms() SystemTime 每请求 ≥3 次
- **现状**：`SystemTime::now()` 封装（`wasm/host_state.rs:11-16`）在 `pipeline.rs:133,239,250,269`、
  `pipeline_peer.rs` 调用，≥3 次/请求（+成功/失败记录各一）。
- **影响**：~1–2µs/req + 语义混杂（墙钟 vs 单调钟混用）。
- **建议改法**：管线入口采样一次传参；单调钟用于时长，墙钟只留给插件可见的 `now_ms` host import。
- **风险/兼容性**：无。
- **预期收益量级**：−1–2µs/req（小，顺手项）。

### N7 无插件时响应流仍锁 session mutex
- **现状**：响应体恒走 `FilteredBody` + `Arc<Mutex<RequestSession>>`，每 chunk `run_filter` 拿锁
  （`body_filter.rs:59-64,95-101`），客户端断开时 `Drop` 兜底再跑（`:143-162`）。
- **影响**：0 插件时每 chunk 一次锁 + 包装层 poll 开销，纯转发路径为插件机制付税。
- **建议改法**：无插件（或无 body_filter/log 插件）时直通 `Incoming`，字节计数用原子/独立小状态；
  log 阶段无插件时跳过。
- **风险/兼容性**：需保证热重载后新插件出现时路径切换正确（reload 已重建 runtime，天然成立）。
- **预期收益量级**：流式/大响应场景每 chunk 开销归零（小到中）。

### N8 h2c 嗅探无 http1_only 快路径
- **现状**：每连接经 `auto::Builder` 嗅探 h2 preface（`h2c.rs:37-94`），无按部署关闭嗅探的配置面。
- **影响**：纯 HTTP/1.1 下游（经典 LB/侧车场景）每连接多付嗅探与分支成本；连接 churn 场景
  （如对端 keepalive_requests 上限）放大。本轮矩阵为长连接，未单独量化——留档为配置缺失项。
- **建议改法**：`[server] http1_only = false` 配置面，true 时走 http1 Builder 直连。
- **风险/兼容性**：默认 false 保持现状；h2c 演练（integration.sh）不受影响。
- **预期收益量级**：每连接常数级（小），主要价值是部署语义明确（迭代 A 低风险开关）。

### N9 元数据字符串重建
- **现状**：`route.clone()` 克隆整个 RouteConfig（`pipeline.rs:114`）；method/path/query/version
  各一次 `to_string`（`:86-99`）+ `method_str.clone()`（`:120`）；每 attempt `client_ip` clone、
  XFF `join(", ")`、`Host = peer.addr.to_string()`（`pipeline.rs:201,225-231`、`forward.rs:82-90,127-132`）。
- **影响**：每请求 ~10 次小分配中与头部无关的部分，3–6µs/req。
- **建议改法**：`Arc<RouteConfig>`；XFF 在 client_ip 不变时按连接缓存；Host 字符串预格式化进 peer。
- **风险/兼容性**：无外部契约。
- **预期收益量级**：−3–6µs/req（小，聚合可观）。

### N10 未解释的 +0.34–0.48ms 单跳延迟
- **现状**：c=100 下 or-warn p50 比 nginx-off 高 0.34–0.48ms 而 CPU 差只有 57µs/req；syscall
  实验三者均 1.00 reads/req，排除响应包化差异。
- **影响**：每请求延迟预算的大头不在 CPU 而在等待/唤醒——候选：accept→parse→pipeline→client→forward
  多跳 task 唤醒链（tokio 调度排队）vs nginx 单线程状态机。
- **建议改法**：`perf`/`tokio-console` 火焰图定位唤醒链；验证 `spawn_per_connection` 与内联处理的
  调度差；对照 nginx worker 模型评估 `worker_threads` 配置面（第一轮 §3 已列）。
- **风险/兼容性**：纯诊断项，无代码风险。
- **预期收益量级**：未知；若半数延迟可消，p50 追平 nginx（大）。

### N11 压测器天花板与方法论基线
- **现状**：单进程 asyncio 压测器 ~44k RPS 封顶；闭合环路下 p50 是 Little 定律反射；本轮绝对值均保守。
- **影响**：所有 RPS/延迟结论只能横向比较；天花板附近的场景（bare/nginx）相互不可分辨。
- **建议改法**：多进程分片压测器（同脚本 fork N 份汇总）或引入 C 压测器复核；补 open-loop
  （固定速率）模式看过载行为。
- **风险/兼容性**：无。
- **预期收益量级**：绝对值可信度（方法论）。

### N12 echo_upstream 未设 TCP_NODELAY
- **现状**：`examples/echo_upstream.rs` 用 `axum::serve`，server 侧 socket 实测无 nodelay
  （`ss` 检查；网关侧 `h2c.rs:72` 已设）。
- **影响**：本轮请求/响应均单段未触发 40ms 台阶；多段响应（SSE/大 body/分块）场景存在 Nagle×delayed-ACK 风险。
- **建议改法**：示例改手动 accept 循环 `set_nodelay(true)` 后 `serve_connection`。
- **风险/兼容性**：仅 examples。
- **预期收益量级**：消除基准工具自身的延迟伪影（测试基建）。

## 5. 行动顺序更新（迭代 A 吸收 N2/N3/N8）

第一轮批次 1 的顺序按本轮实测重排（µs/req 为实测或结构推断标注）：

| 序 | 项 | 实测依据 | 量级 |
|---|---|---|---|
| A1 | §1.1 + **N3** 日志（non_blocking + 生产默认 warn + 去 path.clone） | +29.9µs/req、−13% RPS | 大 |
| A2 | **N2** metrics 单临界区 + label 传参 | 3–4 锁/请求 | 小–中 |
| A3 | §1.3 + **N4** pool_timer + 预解析 client 表 | 1 锁/请求 | 小 |
| A4 | **N8** `http1_only` 配置面 | 每连接常数 | 小（语义） |
| A5 | §1.5 LimitNOFILE + **N6** now_ms 收敛 | — | 小 |

批次 2 顺序不变，§2.1（实例化/PoolingAllocator）最优先——kv-probe +499µs/req、p99 916ms
是全矩阵最大单项；N1（头部物化）升为批次 2 的第二目标（与 §2.3 的 ABI 演进合并设计），
N5/N7/N9 随批次 2 顺带；N10 火焰图作为批次 2 的前置诊断。

## 附：复现

```sh
bash scripts/bench.sh                # 全矩阵，结果落 build/bench/
DURATION=3 CONCURRENCY="100" bash scripts/bench.sh   # 冒烟
python3 scripts/bench_loadgen.py --target 127.0.0.1:9001 --path / -c 100 -d 5
python3 scripts/bench_loadgen.py --target ... --extra-headers 20 ...  # 头部成本探针
```

依赖：本机 3 个 `openrusty-echo@900X` 单元、nginx 二进制（私有实例，pid 隔离）、
`ulimit -n 65535`、`target/release/openrusty`、`build/plugins/kv-probe.wasm`。
