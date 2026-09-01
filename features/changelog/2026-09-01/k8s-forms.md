# K8s 三形态落地：sidecar 透明拦截 / ingress watch / egress 三态（M0.1–M5，21 commits）

## Context
- 网关此前只有明文单端口接入 + 配置文件路由；要进 mesh 需要三块能力：pod 内透明拦截（sidecar）、集群 Ingress 动态采用（ingress）、出站策略（egress）。本批次把三形态一次打通到可演练、可部署、可观测，权威运维面文档为 [docs/sidecar.md](../../../docs/sidecar.md)。

## Change Summary
### A. 存量落库（M0.1）
- **apply_config 构造器 + lib 拆分**（bc3b172）：`state::from_config` 成为二进制/测试/embedder 共用的唯一 AppState 构造路径；网关整体移入 lib target（`openrusty_server`），main.rs 只留薄壳。
- transparent 测试移入子模块（691c4f4），守住 400 行文件上限。

### B. 透明拦截原语 + netns 演练
- **SO_ORIGINAL_DST**（a72f513）：conntrack 恢复 REDIRECT 前的原始目的地（v4/v6）。
- **raw 协议探测**（52dde42）：H1（method token）/ H2（`PRI *` 5 字节）/ Opaque；超时=Opaque（带已读字节），截断 EOF=关连接；嗅探只借字节，前缀一律回注。
- **TCP 隧道 + 回环守卫**（1009a31）：opaque 双向 splice；守卫比对全部自身监听端口。
- **透明拦截拆分**（d841f3a）：transparent 监听器按 role 路由——入向嗅探后 HTTP 进管线（插件照常生效）/ opaque 隧道，入向 orig_dst 不可得降级普通管线、出向关连接；**回环守卫 fail-fast：非 REDIRECT 环境拒全部连接（误配即刻显形）**。
- **netns 演练**（d15cee3/ad1ae38）：一次性网络命名空间里跑真实 iptables REDIRECT，17+ 项断言（后拆 `lib-netns.sh` 共享库并新增 egress 演练）。脚本期揪出并修复 2 个真缺陷：
  - **隧道吞前缀**（7cc2b1d）：opaque 分支把裸 socket 交给隧道，嗅探借走的前导字节被吞（TLS ClientHello 每次都被打坏）；`PrefixedStream` 上提后两分支结构性地拿到回注流，配 4096 随机字节逐字节回归。
  - **幂等断言误报**（ad1ae38）：`iptables-save` 输出带时间戳注释行，两次跑必然 diff 出差异；改为过滤 `^#` 后再比对。

### C. 多监听（a4573f9）
- `[[server.listeners]]` 按 role（inbound/outbound/admin）声明监听面：每 role 至多一个、地址不重复；`http1_only`/`transparent`/`detect_timeout_ms`/`tls` 字段面；admin 存在时 `/openrusty/*` 只挂 admin socket。空 listeners 从 `server.listen` 派生单 inbound（完全向后兼容，永不透明）。

### D. 三段式 shutdown（e5347ca）
- SIGTERM/SIGINT 与 `POST /openrusty/shutdown` 翻同一 watch flag：停 accept → 有界 drain（`shutdown_grace_ms`，默认 5000，10ms 轮询 in-flight）→ 汇总日志退出 0。`GET /openrusty/ready` draining 时 503 `{"status":"draining"}`；`GET /openrusty/live` 恒 200（draining 不该被重启）。

### E. openrusty-k8s crate + watch + render（436a88a/9564bbe）
- 新 crate `crates/openrusty-k8s`（server 消费，依赖箭头指向它）：kubeconfig/`$KUBECONFIG`/`~/.kube/config`/in-cluster 四级凭据加载（rustls apiserver 客户端，basic-auth 用户解析但拒绝）、Ingress/Secret 模型与 watch 流信封、不可变快照状态机（`apply_event` 折叠、只换不清）、`watch_loop` 驱动（LIST→WATCH→410 re-list，退避 100ms→30s，防抖 200ms）、class 过滤渲染（exact/host 路由、ClusterIP upstream、TLS 映射、冲突即硬错）。

### F. ingress 闭环 + TLS 终结（bbc6e3a/cc39f97）
- server 侧接线：每资源路径一个 watch task，第二 200ms 防抖窗合并后 render+apply；**任何渲染错误或 static∩rendered 路由键冲突拒绝整次 apply、旧运行时保持权威（stale-serve）**；成功按当前插件代数原子发布（换路由不重编译插件）。凭据不可用只记错并留在静态配置。
- 监听器 TLS：rustls + ALPN，共享 SNI 动态解析器；ingress Secret 按 host 命中优先、静态对兜底 SNI miss、两者皆无握手失败；轮换只影响新握手，不断在途。渲染 upstream `ing-{ns}-{svc}-{port}` 直拨 `svc.ns.svc.cluster.local:port`，被动健康禁用（`max_fails = 0`）。

### G. exact + host 路由（7e865b3/b751c52）
- `[[routes]]` 新增可选 `host`（去端口、大小写不敏感）与 `exact`（nginx `location =` / Ingress `pathType: Exact`）：host 类先于路径匹配，exact 在同类内绝对胜出。

### H. egress 三态 + 观测（48188f6）
- `[egress]`：`direct`（默认，逐字节隧道到 orig_dst）/ `gateway`（H1/H2 明文字节透传到 gateway，opaque 与 443 目的地拒绝——明文跳变不承载 TLS purpose；dial 失败/超 3s fail-close）/ `deny`（全拒）。`gateway` 为 `host:port`、不携带凭据，config-load 时校验可解析。
- 观测契约：`openrusty_transparent_conns_total{role,outcome}` 八种 outcome 覆盖全部处置（http/tunnel/loop_rejected/no_orig_dst/egress_direct/egress_deny/egress_gateway_ok/egress_gateway_fail）。

### I. init + inject + chart（5dc927d/ef7e806/f19210a）
- `openrusty iptables-init`：nat REDIRECT 参数面（`OPENRUSTY_IN`/`OPENRUSTY_OUT` custom chain、`--proxy-uid` owner RETURN 第一条、幂等 -N/-F/-A + `-C` 守卫挂钩、conntrack/后端/REDIRECT 写删三重 preflight、`--dry-run` 纯计划），linkerd proxy-init 参数面对照。
- `openrusty inject`：stdin 单 YAML 文档 → stdout 注入清单 + ConfigMap（注解面 `config.openrusty.io/*`；v1 边界：仅 Pod/pod-template 类、opaque-ports 仅透传 env、已注入拒绝）；ConfigMap 为最小可启动配置（`[plugins] dir = "/dev/null-plugins"` 视为无插件）。
- Helm chart `deploy/charts/openrusty` 三件套（ingress：Deployment+LoadBalancer 8443 明文+admin+probe；egress-gateway：pause+静态 baked 透明 sidecar、`[egress] deny` 托底；demo 默认关）+ `scripts/chart-lint.sh`（3 release 渲染 + chart-vs-CLI sidecar 一致性演练）。

## 测试覆盖
- `cargo test --workspace`：442 通过。
- `scripts/integration.sh`：102/102。
- `scripts/local-netns-test.sh`：26/26；`scripts/local-egress-test.sh`：21/21（真实 REDIRECT/netns）。
- `scripts/chart-lint.sh`：24/24（无集群、无 kubeconfig）。

## Impact Surface
- 新配置面（均有 serde default，旧配置零改动可跑）：`[[server.listeners]]`（写入即整体接管，`server.listen` 变死配置并告警）、`[egress]`、`[ingress]`、`[[routes]]` 的 `host`/`exact`、`server.shutdown_grace_ms`。
- 新端点：`GET /openrusty/ready`、`GET /openrusty/live`、`POST /openrusty/shutdown`；`/openrusty/status` 新增 `ingress` 节；`/openrusty/metrics` 新增 `openrusty_transparent_conns_total`。
- 新子命令：`openrusty iptables-init`、`openrusty inject`（均一次性、不碰运行中的网关）。
- 行为不变式保持：默认（无 listeners、无 `[egress]`、`[ingress]` 关）与旧单端口部署逐字节兼容。

## Related Docs
- [docs/sidecar.md](../../../docs/sidecar.md)（权威运维面）、[docs/inject.md](../../../docs/inject.md)
- [K8s 三形态](../../k8s-forms/index.md)、[运维与部署](../../ops/index.md)
- [网关请求管线](../../../agents/gateway-pipeline/index.md)
