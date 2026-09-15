# 集群侧生产就绪（P0–P5）：k3s 实测收敛到全绿

范围：cluster-e2e 七组断言在单节点 k3s（v1.36.4+k3s1）上首次全绿（61 PASS / 0 FAIL / 1 deliberate SKIP，2min 快速窗）；次日以最终镜像按 **M4 验收形态（默认 10min 窗）复跑：63/0/1 全绿**（新增 2 项计数断言 + 1 个未计数守卫路径见下）。

## 产品修复（均带回归测试）
- k8s client HTTPS：`HttpConnector` 未 `enforce_http(false)` 导致 https URI 报 "invalid URL, scheme is not http"（hyper_util legacy Display 会吞 source，须打印 error chain）。`crates/openrusty-k8s/src/connector.rs`。
- chart RBAC 语义：`ingress.watchNamespaces`（默认收敛到 Release namespace）+ `rbac.clusterWide`（ClusterRole/Binding），与产品"空 namespaces = 全集群"语义对齐；chart-lint 增至 30 checks。
- Ingress 上游 DNS：`endpoints`（`svc.ns.svc.cluster.local:port`）此前被静默丢弃（peers=0）；新增 `openrusty-proxy::resolve`（apply 时解析、失败 warn-only 保留旧 peers），`reload`/`ingress apply_pair` 接线。
- HTTP/2 `:authority` 未折叠进 Host 匹配（host 约束路由 h2 404）：`fold_h2_authority` 于 header 收集后。
- **watch resync 看门狗**：`WatchOptions.resync`（默认 30s）周期性重启 LIST→WATCH，半开 TCP 死流不再长期钉住旧快照（M4 "fresh LIST on record" 验收）；rv 未变的 re-list 不触发下发（安静集群不搅动 generation）。

## 演练（drill）修复 —— 误报类别存档
- `/openrusty/status` 的 ingress 节是**扁平**字段（仅 `secrets` 嵌套）；脚本曾按 `ingresses.*` 子路径读取（空读），连带 G5 用 `""==""` 伪通过。
- 单发探针 vs 双副本 LB：收敛窗口内的 withdrawal/serving 断言改为有界 `wait_for`；负向断言用 `route_never_serves`（连续 N 次未命中）。
- 断网演练曾用 `egress: []` 全量断网（连带断后端，靠连接池余温侥幸通过）；改为 ipBlock except 仅断 apiserver（ClusterIP + 节点 IP），数据面保持存活。
- admin port-forward 隧道自愈（`admin_curl`/`status_field` 空读时重建）；失败时 `dump_state`（对象 + status + pod 日志）落进日志。
- Prefix 语义陷阱：`/pre` Prefix 天然覆盖 `/pre2`，"withdrawn path 不复活"类断言不能拿它当反例。
- **write_cut 运行期自检**（复核新增）：断网策略必须以 ipBlock/except 形态生成（IP 发现失败显式 SKIP，不再静默回退 deny-all——该回退会让恢复类检查在 no-op cut 上平凡通过）；apply 后再断言集群上的策略确实保留 except 列表。
- **G7 双副本直采**（复核新增）：deploy 级 port-forward 只钉住一个 pod，wedge 的第二副本会被健康副本掩盖；G7 改为对每个 Running 网关 pod 建独立 admin 隧道，ready/live/watching/last_success_age_ms 逐 pod 断言（10min 窗 60 样本 × 2 pod 全绿）。

## 验证基线（2026-09-15/16）
workspace 525/0（复跑一致）· integration 135/135 · netns 26/26 · egress 21/21 · chart-lint 30/30 · sidecar-e2e 32/32（含新增 S6 chart demo gate）· cluster-e2e 61/0/1（2min 窗）→ 63/0/1（默认 10min 窗，最终镜像，2026-09-16 复核）。

## 遗留红线（不阻塞 GO，需运维边界声明）
fail_open 默认 · `/openrusty/*` 无鉴权 · TLS-over-transparent 未落地 · Ingress admission webhook 缺位 · 镜像仅本地构建通道（无 registry 发布）。
