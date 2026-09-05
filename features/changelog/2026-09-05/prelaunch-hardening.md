Commit: 86fac98
# 上线前收敛：锁中毒加固、文档对齐、运营红线、netns 修复

## Context
- Part 1 上线前最终确认的产物：全量验证门跑绿（见下表）+ 三类小修。无功能语义变化，除 lib-netns 修复外均为加固/文档。

## Change Summary
- **锁中毒加固**：生产路径的 `Mutex::lock().unwrap()` / `.expect(...)` 统一为 `unwrap_or_else(|e| e.into_inner())`（先例 `host_state.rs lock_cursors`）——持有锁的线程 panic 后网关继续可用而不是连锁 panic。覆盖 `metrics.rs`(8)、`health.rs`(10)、`client.rs`(2)、`active_probe.rs`(2)、`pipeline_peer.rs`(1)、`ingress/mod.rs`(1)、`host_state.rs`(2)。测试代码保持 `.unwrap()`。
- **文档数字对齐**：`README.md` / `features/ops/index.md` 的 integration checks 数从快照值（93/102）对齐脚本动态输出（现 119，随 §30/§31/§32 增补同步）。
- **上线红线落档**：`features/ops/index.md` 新增「上线红线（部署前必读）」：fail_open 默认放行、`/openrusty/*` 无鉴权、静态证书 reload 不重读、16MiB body 全内存缓冲。
- **netns drill 修复**：`lib-netns.sh` 生成的 veth 名（`veth-egr-$$`）在 7 位 PID 机器上超 IFNAMSIZ(15)，iproute2 以晦涩的 `Attribute failed policy validation.` 拒绝、egress drill 死在拓扑阶段。截断至 15 字符修复。

## 验证门（冻结快照上执行，快照与最终代码面零漂移）
| 门 | 结果 |
|----|------|
| `cargo test --workspace` | 497 passed / 0 failed（原文误记 525；实测 workspace 497 + 插件单测 27 = 524，两口径均非 525） |
| `cargo clippy --all-targets -- -D warnings` | 0 warning |
| `scripts/build-plugins.sh` | 4 插件构建+单测通过 |
| `scripts/integration.sh` | 119/119 |
| `scripts/local-netns-test.sh` | 26/26 |
| `scripts/local-egress-test.sh` | 21/21（需先落 lib-netns 修复） |
| `scripts/chart-lint.sh` | 24/24 |
| `scripts/cluster-e2e.sh --preflight` | PASS 4 / FAIL 0 / SKIP 6（环境无 kubectl/kubeconfig，G2-G7 集群断言待集群可用补跑） |

## Impact Surface
- 运行时行为不变（锁加固只影响中毒路径）；drill 修复让 egress 门在高 PID 宿主机可跑。

## Related Docs
- [运维与部署](../ops/index.md)（红线与验证计数）
