# cluster-e2e.sh：M4 集群 e2e 七组断言载体（2bff10d）

## 背景
三形态的本地验收面（442 单测 / integration 102 / netns 26 / egress 21 / chart-lint 24）已全绿，但 M4（集群 e2e 七组断言）没有执行载体：集群行为维度（真集群路由+TLS 握手、真 apiserver watch 韧性、摘注解回滚、观察窗）零证据。需要一个无集群时可预检、集群就位后可全量验收的脚本，把 M4 验收动作固定下来。

## 变更
### 测试载体
- **`scripts/cluster-e2e.sh`**（397 行，executable，纯 ASCII）：七组断言，红线出处逐组锚定 docs/sidecar.md（脚本头注释）：
  - G1 预检（:146）：RBAC get/list/watch ingresses+secrets 逐动词实测、server ≥1.19、镜像可解析；每项缺口具名进 GAP 清单（kubeconfig 就位前即可交付权限缺口清单）
  - G2 部署可达（:200）：helm 装 ingress release；LoadBalancer 120s 无地址自动降级 NodePort 并记录；`/openrusty/ready` + admin 4191 port-forward
  - G3 路由+TLS（:238）：Exact/前缀/host 约束；自签 Secret + ConfigMap `tls=false→true` 补丁 + rollout restart；openssl SNI 证书 CN 校验；host 不匹配不命中
  - G4 watch 韧性（:281）：改路径生效→删除撤下；NetworkPolicy 断网 12s stale-serve（快照只换不清）→恢复 re-list 收敛（generation/last_rv/reconnects 经 `/openrusty/status` 观测）；RBAC 不允许时显式 SKIP，不算失败
  - G5 冲突红线（:314）：同 (host,path) 键 / TLS Secret 缺失 → 整次 apply 被拒：旧路由保持权威、generation 不前进
  - G6 摘注解回滚（:335）：删除 Ingress 全程 ready 采样 0 miss、restartCount 不增、无 CrashLoopBackOff
  - G7 观察窗（:350）：默认 10 分钟（`--window-min` 仅限本地迭代；M4 验收必须用默认值），ready/live/watching 每采样点全绿
- **门禁与退出**：`--preflight` 无集群可跑（缺口具名 + G2–G7 逐组 SKIP + exit 0）；无基础工具时全量模式走同一 SKIP 门禁（三兄弟脚本惯例）；集群就位后 G1 硬缺口 → 缺口清单 + exit 1（无静默通过通道）
- **清理**：drill 资源按 `app.kubernetes.io/orr-e2e=yes` 标签 + helm uninstall，trap EXIT best-effort，不掩盖断言结果

## 测试覆盖
| 功能 | 测试 | 结果 |
|------|------|------|
| 预检模式（无集群） | `./scripts/cluster-e2e.sh --preflight` | PASS 4 / FAIL 0 / SKIP 6，exit 0 |
| 全量模式门禁（无集群） | `./scripts/cluster-e2e.sh` | SKIP 路径 exit 0 |

- 伪绿审计：G4 NetworkPolicy podSelector 与 chart `_helpers.tpl` selectorLabels（`name=openrusty` + `instance`）逐标签对表匹配；`check()` FAIL 分支与 exit 1 iff FAIL>0 核实
- 全量回归：`cargo test --workspace` → 442 通过（3 ignored 与基线一致）；integration 102/102；netns 26/26；egress 21/21；chart-lint 24/24
- clippy：`cargo clippy --workspace --all-targets -- -D warnings` → 零警告
- 行数：`scripts/cluster-e2e.sh` 397 ≤ 400

## Impact Surface
- 纯测试载体：不进发布二进制，公开接口/配置形状零变化
- M4 验收命令固定为 `./scripts/cluster-e2e.sh`（默认 10 分钟窗）；交付物是权限/环境缺口清单 + 七组断言结果

## Related Docs
- [K8s 三形态](../../k8s-forms/index.md)
- [docs/sidecar.md Local drills](../../../docs/sidecar.md)（红线出处）
- [k8s-forms 批次 changelog](./k8s-forms.md)
