# K8s 三形态：sidecar / ingress / egress

## 能力概述
- 同一个 `openrusty` 二进制，纯靠配置分成三种部署形态：sidecar（pod 内透明拦截）、ingress（watch Ingress + TLS Secret 动态路由）、egress（出站三态）。HTTP 连接在任一形态都走同一 8 阶段插件管线；opaque TCP 走明文隧道（不经 wasm 阶段）。
- 权威运维面文档：[docs/sidecar.md](../../docs/sidecar.md)（端口、决策矩阵、watch 语义、部署面、安全边界均以此为准）。

## 完成定义

### sidecar
- `[[server.listeners]]` 按 role（inbound/outbound/admin）拆分监听面；空 listeners 时从 `server.listen` 派生单 inbound（向后兼容）。
- `transparent = true` 的监听器跑拦截路径：SO_ORIGINAL_DST 取原始目的地 → 回环守卫（比对全部自身端口，fail-fast；非 REDIRECT 环境会拒绝全部连接）→ 协议探测（H1/H2/Opaque；截断 EOF 关连接；超时按 Opaque）→ HTTP 进管线 / opaque 进隧道。
- 入向 orig_dst 不可得降级为普通 HTTP 管线；出向不可得直接关连接。
- 验收入口：`scripts/local-netns-test.sh`（26 checks，真实 REDIRECT + netns）。

### egress
- `[egress]` 三态：direct（默认，逐字节隧道到原始目的地）/ gateway（H1/H2 明文字节透传到 `[egress].gateway`；opaque 与 443 目的地拒绝）/ deny（全拒，fail-close）。gateway 拨号失败或超 3s 关连接。
- 观测契约：`openrusty_transparent_conns_total{role,outcome}` 覆盖全部处置。
- 验收入口：`scripts/local-egress-test.sh`（21 checks，direct/deny/gateway 三阶段 + 指标断言）。

### ingress
- `[ingress]`（enabled/ingress_class/kubeconfig/namespaces）：watch `networking.k8s.io/v1` Ingress + `kubernetes.io/tls` Secret，渲染为动态路由。
- watch 语义：LIST→WATCH→410 re-list，指数退避 100ms→30s，200ms 防抖；**stale-serve：快照只换不清**，apiserver 不可达时继续用最后快照；周期性 resync：每 30s 重启 LIST→WATCH 周期，静默死流（半开 TCP）也不会长期钉住旧快照，rv 未变的 re-list 不触发下发。
- 冲突策略：static∩rendered 路由键（host+path_prefix+exact）冲突 → 拒绝整次 apply、保留旧运行时；TLS Secret 缺失/类型不对同罪。
- TLS：SNI 动态证书（ingress 命中优先，静态对兜底 SNI miss；两者皆无则握手失败）；轮换不断在途连接。
- upstream 直拨 ClusterIP DNS（`ing-{ns}-{svc}-{port}` → `svc.ns.svc.cluster.local:port`），被动健康禁用（`max_fails = 0`）；路由支持 exact + host 约束。
- 凭据不可用时仅记录错误、留在静态配置——ingress 是增强不是依赖。
- 观测面：`/openrusty/status` 的 `ingress` 节（enabled/watching/generation/last_rv/reconnects/last_success_age_ms + secrets）。
- 验收入口：`scripts/cluster-e2e.sh`（M4 集群 e2e 七组断言 + 10min 观察窗；`--preflight` 无集群可跑，见 [changelog](../changelog/2026-09-01/cluster-e2e.md)）。

## 部署面
- `openrusty iptables-init`：nat REDIRECT 参数面（OPENRUSTY_IN/OPENRUSTY_OUT custom chain、owner 豁免第一条、幂等、preflight 自检、--dry-run）。
- `openrusty inject`：静态注入（v1 边界：单 YAML 文档、opaque-ports 仅透传 env），见 [docs/inject.md](../../docs/inject.md)。
- Helm chart 三件套（ingress / egress-gateway / demo），`scripts/chart-lint.sh`（30 checks）+ chart-vs-CLI 一致性演练。
- 生命周期：三段式 shutdown（SIGTERM 与 `POST /openrusty/shutdown` 同一路径）；`/openrusty/ready` draining 时 503、`/openrusty/live` 恒 200；`shutdown_grace_ms`（默认 5000）。

## 不含什么（P2/P3 边界）
- tap/流量观测、multicluster、Gateway API、admission webhook、endpoint 级负载均衡（当前直拨 ClusterIP，kube-proxy 负责 per-pod 分发）。
- egress gateway 上的路由/策略面（chart 中暂以 `[egress] mode = "deny"` fail-close 托底）。
- TLS-over-transparent、出向 TLS 终结、chart 渲染 RBAC 对象（随真实镜像通道落地）。

## 关联
- 语义文档：[docs/sidecar.md](../../docs/sidecar.md)、[docs/inject.md](../../docs/inject.md)
- 模块：[网关请求管线](../../agents/gateway-pipeline/index.md)
- 变更记录：[changelog 2026-09-01](../changelog/2026-09-01/k8s-forms.md)
