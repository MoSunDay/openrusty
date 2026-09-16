# 管理面 token 鉴权 + CI 门禁 + least_conn 均衡

## Context
- 三线交叉验证（OpenResty 反代能力对标、WASM 动态加载、“随时可上新”闭环）确认主线能力成立，遗留三处可补强：① 三层验证全靠手动脚本，无 CI 门禁；② `/openrusty/*` 管理端点无鉴权（ops 红线，仅靠 loopback/admin listener 隔离）；③ 负载均衡缺 least_conn。
- 决策：热更新触发器**保留显式 reload**（SIGHUP/POST，不加 fs watcher——自动 reload 的爆炸半径大于收益），以 CI 门禁兜底闭环；鉴权选共享 token（mTLS 留待有实际需求）；least_conn 按 nginx 语义（in-flight/weight）实现。

## Change Summary
### A. 管理面 token 鉴权（可选，默认关闭，零行为变化）
- 新配置节 `[admin]`（`crates/openrusty-core/src/config/admin.rs`）：`token` 缺省/留白 = 不鉴权，历史行为与全部既有测试/演练不变。
- 新中间件 `crates/openrusty-server/src/admin_auth.rs`（挂 `admin_routes()` 的 `route_layer`，三处挂载形态共同收口）：`status/reload/metrics/shutdown` 要求 `Authorization: Bearer <token>` 或 `X-OpenRusty-Token: <token>`（常量时间比较）；`ready`/`live` 豁免（LB/k8s 探针）；reload 的 loopback 403 在鉴权之后叠加保留。
- token 轮换只在文件型 reload 成功路径（`reload.rs::reload`）更新 `AppState.admin_token`（`ArcSwapOption`）——ingress 渲染/内嵌配置无 `[admin]` 节，不得清空 boot token。
### B. CI 门禁
- `.github/workflows/ci.yml`：三 job = `cargo test --workspace --locked` / `scripts/build-plugins.sh` / `scripts/integration.sh`（断言退出码，不断言 check 数；预装 wasm32 target + rust-cache）。fmt/clippy 当前树不干净（k8s crate clippy error、多文件 fmt drift），暂不入门禁，补齐后可加回。
### C. least_conn 均衡
- `BalancerKind::LeastConn`（TOML `balancer = "least_conn"`）。
- 纯函数 `proxy::least_conn_pick`：整数交叉相乘比较 in-flight/weight（免浮点），平局取最小下标。
- in-flight 计量挂 `HealthRegistry::PeerHealth`（新增 `AtomicUsize`，**按地址键控**、随 `register` 的地址 remap 存活 reload）：`pick_peer` 两处 `Pick::Peer` 返回点 inc；`pipeline`/`ws` 每次尝试收尾（Ok/Err/timeout 三臂）经 `release_peer` dec。
### D. e2e 演练
- 新增 §33 管理面 token 矩阵（默认关闭兼容、401/放行、探针豁免、token reload/shutdown，12 checks）与 §34 least_conn 并发分布（5 checks）；总数 135 → 152。

## 测试覆盖
| 面 | 载体 |
|----|------|
| `[admin]` 解析/默认/deny_unknown_fields/留白禁用 | `config/admin.rs` 单测 4 例 |
| 鉴权矩阵（无 header 401、双 header 放行、探针豁免、loopback 叠加 403、默认关闭兼容 200） | `admin_auth.rs` 单测 7 例；演练 §33 12 checks |
| least_conn 纯算法（空集/平局/避忙/权重倾斜/确定性） | `balancer.rs` 单测 5 例 |
| in-flight 计量（地址键控、reload remap 存活、下限钳 0） | `health.rs` 单测 1 例 |
| 选择集成（忙 peer 避让、release 后回落） | `pipeline_peer.rs` 单测 1 例；`config.rs` 解析 1 例 |
| 全量回归 | `cargo test --workspace` 544 通过；e2e 152/152 |

## Impact Surface
- 默认路径零变化：无 `[admin]` ⇔ 旧语义；drill 主网关不配 token，全部旧 check 原样通过。
- §34 首版踩坑（已修）：`/slow?ms=400` 与 §13 早前注入的 `/slow` 路由（`timeout_ms = 300`）冲突 → 12 请求全部 `502 upstream timeout`；改为 `ms=150`（低于该路由超时、仍构成 in-flight hold）。另将节点计数 python 的崩溃兜底为 `|| echo 0`——`set -e` 下命令替换失败会中断整个 drill。
- 已知未做：fs watcher（有意保留显式 reload）、按响应码（5xx）failover、fmt/clippy 门禁、上游 h2、请求体 16MiB 上限。

## Related Docs
- [features/ops/index.md](../ops/index.md)（红线改写：鉴权可选；152 checks；CI 门禁）
- [features/load-balancing/index.md](../load-balancing/index.md)、[agents/proxy/index.md](../../agents/proxy/index.md)（least_conn 语义）
- [docs/wasm-abi.md](../../docs/wasm-abi.md)（reload 端点鉴权注记）
