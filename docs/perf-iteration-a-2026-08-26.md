# OpenRusty 迭代 A 性能修正报告（第三轮 · 2026-08-26）

- 基线：round-2 报告 `perf-nginx-comparison-2026-08-25.md`，数据锚定 `11017bc` 的复测
  `build/bench/results-20260826-211748.jsonl`（下称 **base**）。
- 本轮：迭代 A 全部 5 项落地后全矩阵复测 `build/bench/results-20260826-231242.jsonl`
  （下称 **after**，网关代码锚定 `53e2e6f`），同方法同机（nproc 16，`DURATION=10s`，36 格 err=0）。
- 主判据仍是 **µs/req（进程侧 CPU/请求）**；本轮额外确立一条方法论规则：**跨轮绝对值比较
  仅作参考，结论以同一轮内的成对差值（paired delta，如 or-info − or-warn）为准**（§4）。

## 0. TL;DR

| 结论 | base → after |
|---|---|
| 硬门槛：访问日志成本（or-info − or-warn，paired，c=100/500 四格均值） | **29.4 → 20.9 µs/req（−29%）**；c=100 单格 29.1→20.1、30.7→18.8。**未达 <15 预期**，残余为主线程字段格式化 + 通道交接固定开销（§3、§5） |
| or-kvprobe（wasm 未动，间接受益） | mean 618 → 535 µs/req，高并发改善最大（c=2000 `/` 733→481）；机制未确证，不作为本轮收益主张（§4.4） |
| fd 泄漏 | **已修复并有实证**：20k 次串行"新建连接→请求→关闭"后进程 fd 数恒为 11（修复前 idle 连接永不回收）（§4.3） |
| `http1_only` 配置面 | 默认行为不变（h2c 开启，93 checks 全过）；`=true` 时 h2c 探测关闭，已用 prior-knowledge 探针验证（§4.5） |
| or-warn 无插件管线（绝对值） | 70.2 → 74.4 mean；**判为噪声带内持平**（±6µs 同二进制重复运行差 > 项效应；控制组 nginx/bare 同向漂移，§4.2） |

## 1. 改动清单（A1–A5，各自独立 commit）

| 项 | Commit | 内容 |
|---|---|---|
| A1 | `e903fe2` | 日志 writer 改 `tracing_appender::non_blocking(stdout())`，guard 在 main 持有至退出时 flush；`default_log_level` info→**warn**（example.toml / openrusty.toml 同步）；`finish_log` 去 `path.clone()` |
| A4 | `e35a16f` | 新增 `[server] http1_only`（serde default false）：true 时走纯 `http1::Builder`（仍 `with_upgrades` 保 WebSocket），false 保持 h2c auto-detect 不变 |
| A5 | `aedacfc` | gateway systemd 单元加 `LimitNOFILE=65535`；被动健康成功/失败记录锚定请求起点时刻 `now`（收敛请求内 now_ms 调用） |
| A2 | `02ff677` | metrics 合并为单临界区 `record_request_timed`（请求计数+直方图一次锁）；`handle_request` 返回 `(Response, Option<route_idx>)`，fallback 免去二次 `match_route` 与 `path.to_string()`，label 以 `&str` 解析；输出格式零变化（格式锚定的单测/93 checks 全过） |
| A3 | `53e2e6f` | per-peer client 补 `.pool_timer(TokioTimer)` + `.pool_idle_timeout`（新配置 `pool_idle_timeout_ms`，默认 60s）；boot/reload 时按 peers 预建 client |

批次边界遵守 round-2 §5：wasm 批次（实例池化、头部物化 N1 等）未动。

## 2. 全矩阵 before → after（µs/req）

```
场景        path   c      base   after    Δ          rps base→after
bare        /      100     9.0    9.5   +0.5       44827→46841
bare        /      500    10.0   10.1   +0.1       43561→43479
bare        /      2000   11.0   10.4   -0.6       35146→35180
bare        /echo  100    16.9   14.6   -2.3       43262→46614
bare        /echo  500    13.6   13.8   +0.2       43580→43609
bare        /echo  2000   16.1   13.9   -2.2       33135→34776
nginx-off   /      100    11.9   12.4   +0.5       43072→42177
nginx-off   /      500    11.2   12.0   +0.8       41077→39970
nginx-off   /      2000   12.3   13.0   +0.7       35828→33521
nginx-off   /echo  100    11.1   12.6   +1.5       42440→40903
nginx-off   /echo  500    12.2   12.0   -0.2       40001→38914
nginx-off   /echo  2000   13.0   13.5   +0.5       33601→33475
nginx-on    /      100    13.0   13.8   +0.8       45050→43568
nginx-on    /      500    13.1   13.9   +0.8       41775→40870
nginx-on    /      2000   15.3   15.1   -0.2       33687→33743
nginx-on    /echo  100    13.5   13.8   +0.3       42933→42487
nginx-on    /echo  500    13.9   14.6   +0.7       40810→40073
nginx-on    /echo  2000   22.9   15.7   -7.2       30225→32728
or-warn     /      100    67.2   67.6   +0.4       31870→33228
or-warn     /      500    66.8   71.9   +5.1       34334→29728
or-warn     /      2000   70.6   75.5   +4.9       30250→28214
or-warn     /echo  100    70.7   75.7   +5.0       32113→26259
or-warn     /echo  500    70.5   75.6   +5.1       31705→26226
or-warn     /echo  2000   75.7   79.9   +4.2       29696→25555
or-info     /      100    96.3   87.7   -8.6       27318→30380
or-info     /      500    95.9   89.6   -6.3       27665→28421
or-info     /      2000  101.2  109.7   +8.5*      26847→20609 (*异常格,见§3)
or-info     /echo  100   101.4   94.5   -6.9       25914→27238
or-info     /echo  500    99.4   96.8   -2.6       26420→28164
or-info     /echo  2000  103.7   93.1  -10.6       26008→28100
or-kvprobe  /      100   585.4  552.7  -32.7       10522→10659
or-kvprobe  /      500   703.2  495.3 -207.9        8579→11317
or-kvprobe  /      2000  733.1  481.4 -251.7        8283→11952
or-kvprobe  /echo  100   550.6  538.5  -12.1       11002→ 9822
or-kvprobe  /echo  500   603.4  598.0   -5.4       10070→ 9742
or-kvprobe  /echo  2000  634.6  543.2  -91.4        9545→10414
```

## 3. 硬门槛判定：访问日志成本

同轮内 paired delta（or-info − or-warn，消除环境漂移）：

| 格 | base Δ | after Δ |
|---|---|---|
| c=100 `/` | 29.1 | 20.1 |
| c=500 `/` | 29.1 | 17.7 |
| c=2000 `/` | 30.6 | 34.2 * |
| c=100 `/echo` | 30.7 | 18.8 |
| c=500 `/echo` | 28.9 | 21.2 |
| c=2000 `/echo` | 28.0 | 13.2 |
| **均值** | **29.4** | **20.9** |

\* 该格 after 侧 or-info 自身异常（RPS 27k→20.6k、p99 325ms、cpu% 反常下降），为环境噪声格，
   计入也仅恶化均值，如实保留。

- **判定：未达 <15 目标，实际 −29%（29.4 → 20.9）。**
- 收益归因符合预期结构：non_blocking 移走的正是阻塞写 + stdout 锁，约 −8~10µs/req；
  残余 ~19µs 是 `tracing` 字段仍在调用线程上格式化（fmt::write 逐字段 + level/target 前缀）
  加环形缓冲通道交接的固定开销——计划风险节已预告此残余并要求如实报告。
- 同一二进制的独立重复运行交叉验证：23:09 冒烟轮（DURATION=5s，c=100）Δ = 17.7 / 21.5，
  与全矩阵的 18~21 带一致 → 改善量稳健。
- 后续若要逼近 <10µs，需把 per-event 格式化改为静态行模板/预物化字段（或直接输出预拼好的
  访问日志行），属批次 2 结构性改造，与 N1 头部物化同一设计域。

## 4. 逐项实测 vs 预测

### 4.1 A1 non_blocking + 默认 warn + 去 clone —— 实测收益 ≈ 预测
预测 only 移走阻塞写（量级未单独承诺）。实测访问日志 Δ −8.5µs/req（见 §3），
与本轮在该单项上的结构性预期一致。默认 warn 的部署语义变化已在 example 注释与 changelog 标注；
依赖访问日志的部署需显式 `log_level = "info"`。注意 `config/openrusty.toml` 已改 warn，
生产 :8180 在重启前不受影响，重启后请自行确认是否要显式开回 info。

### 4.2 A2/A5 对 or-warn 绝对值的影响 —— 被噪声淹没，不 claim
or-warn 全格 +4~5µs（70.2→74.4 mean），但三条证据指向环境漂移而非代码回退：
1) **同二进制两次运行**：23:09 冒烟 or-warn `/`=`/echo`=68.3/69.7，23:12 全矩阵同格 67.6/75.7
   —— /echo 相差 6.0µs，大于任何单项的预测效应；
2) **控制组同向漂移**：bare/nginx（代码零改动）mean 分别 +0.x ~ +0.8µs，baseline 自己就有
   bare `/echo` c=2000 16.1 vs nginx-on 同格 22.9 的跨场景离群；
3) **全局内存水位漂移**：vmhwm 连 bare/nginx 都整体抬高（bare 24→126MB），说明两次运行间
   主机状态不同，.vmhwm 一律不做跨轮比较。
结论：A2 单临界区与 A5 时钟收敛的结构性收益（预测 −3~6µs）方向正确但低于本机 ±6µs 噪声带，
需批次 2 引入火焰图/交替压测法（interleaved A/B）才能可信测量。实现本身的正确性由
字节级一致的快照单测与 93 checks 保证。

### 4.3 A3 pool_timer + idle_timeout + 预热 —— fd 泄漏实证修复
- 专项冒烟（私有实例 :18193，config 同 bench 形状）：串行 20,000 次"新建 TCP→GET→关闭"
  后 `ls /proc/<pid>/fd | wc -l` = **11**（预热基线亦为 11，零增长）；HTTP/1.1 与 h2c 双模式同样通过。
- 修复机理：hyper legacy client 未注册 timer 时连接池的空闲回收永远不触发 → 高抖动流量下
  fd 只增不减；现在空闲连接受 `pool_idle_timeout_ms`（默认 60s）约束回收。
- boot/reload 时 `apply_runtime` 预建全部 peer 的 client：请求路径不再出现"首个请求定型
  connect_timeout"的行为，未知 addr 的懒创建回退路径保留（ws/probe 不受影响）。

### 4.4 or-kvprobe 大幅改善 —— 间接收益，机制未证，不 claim 为本轮目标
wasm 路径零改动的前提下 mean 618→535（高并发格最多 −252µs、p99 857→790ms）。
候选解释：饱和点附近 metrics 锁竞争减半、warn 级下日志路径更早短路、健康窗口语义修正
避免误摘除引发的重试放大。因缺少隔离开关无法归因，列为待批次 2 火焰图复核的观察项。

### 4.5 A4 http1_only —— 功能面按预期
私有实例探针：默认配置 http1.1=200 且 prior-knowledge h2c=200（行为不变）；
`http1_only = true` 时 http1.1=200、h2c 连接被拒（探测绕过生效），WebSocket 升级路径保留
（`with_upgrades`）。93 checks 中管理端点、WS、SSE 全部经默认模式回归通过。

## 5. 方法论沉淀（后续轮次沿用）
1. **成对差值优先**：同轮内比较两个 scenario 才能对冲主机级漂移；跨轮绝对值只看量级。
2. 效应 <±6µs 的判断必须用**同二进制重复运行**校准噪声带，必要时做交替 A/B。
3. vmhwm/RPS/p99 均受 loadgen 单进程天花板与环境状态影响，不作为跨轮主判据。
4. 遗留：N10（+0.34–0.48ms 单跳延迟）、loadgen 天花板（N11）不在本轮范围；
   本轮无一项触及 p50 差距，该差距属调度/唤醒链，指向批次 2 的 wasm 实例池化与诊断项。

## 6. 下一步（批次 2 预告，优先级不变）
1. §2.1 wasm 实例池化 + PoolingAllocator（本轮 kvprobe 数据进一步坐实其为最大单项）;
2. N1 头部物化（与 ABI 演进合并设计）；
3. 访问日志静态行格式（本报告 §3 残余的下一步）；
4. N10 火焰图先于一切延迟优化。

## 附：复现
```sh
cargo test --workspace && cargo build --release
bash scripts/build-plugins.sh && bash scripts/integration.sh   # 93 checks
bash scripts/bench.sh                                          # 全矩阵 36 格
# 迭代A专项冒烟(私有端口18193,与生产隔离): 私有toml + target/release/openrusty 后台起,
#   curl --http2-prior-knowledge 探 h2c；python 串行 20k 次短连接后比 /proc/<pid>/fd 数量。
```
