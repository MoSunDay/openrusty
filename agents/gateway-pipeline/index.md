Commit: 08a95ba
# 网关请求管线（openrusty-server）

## 职责
- `openrusty` 二进制：同端口 HTTP/1.1 + h2c 接入、请求路由、8 阶段插件管线编排、WebSocket/SSE 透传、管理端点；K8s 三形态接入面（role 多监听、透明拦截、egress 三态、ingress watch、TLS 终结、iptables-init/inject 子命令、三段式 shutdown），语义详见 [docs/sidecar.md](../../docs/sidecar.md)。

## 边界
- 负责：连接接入与协议识别（`h2c.rs`）；axum 路由（`app.rs`：`/openrusty/*` 优先，其余落入代理管线）；阶段顺序编排与 Decision 处置（`pipeline.rs`）；响应流式处理（`body_filter.rs`）；WebSocket 升级透传（`ws.rs`）；配置路径解析与进程启动（`main.rs`）；role 监听面装配与透明拦截/TLS 分派（`listeners.rs`）；透明拦截协议拆分（`transparent.rs`/`transparent/`）；出站三态策略（`egress.rs`）；TLS 终结与 SNI 动态证书（`tls/`）；ingress watch 接线（`ingress/`，渲染管线在被消费的 `openrusty-k8s` crate）；一次性子命令（`init/`、`inject/`）；统一三段式 shutdown（`shutdown.rs`）。
- 不负责：插件的执行与沙箱语义（[WASM 插件运行时](../wasm-runtime/index.md)）；peer 选择、健康与转发细节（[代理与负载均衡](../proxy/index.md)）；配置结构与 Decision 定义（`openrusty-core`）；k8s 凭据/watch 状态机/渲染（`openrusty-k8s`，依赖箭头指向本 crate 的被消费库，语义由网关条目与 [docs/sidecar.md](../../docs/sidecar.md) 覆盖，不单独建索引）。

## 关键设计
- 路由表：`/openrusty/status`（GET）、`/openrusty/reload`（POST）、`/openrusty/metrics`（GET）先于业务路由匹配，其余请求进入 `handle_request` 代理管线。
- 业务路由按 `path_prefix` 最长前缀匹配；无匹配返回 404。
- 请求侧按 `Phase::PRE_PROXY` 顺序执行 `post_read`/`rewrite`/`access`/`content`；任一阶段出现 `Deny` 即短路返回，`content` 阶段的 `Done` 短路为空 204（插件已自行应答）。
- `content` 的默认处理器是代理转发：每次 upstream 尝试前先运行 `balancer` 阶段（插件可接管 peer 选择）；连接级失败换 peer 重试。
- 响应侧：`header_filter` 一次、`body_filter` 每个数据块一次（`Done` 表示流结束）、请求收尾运行 `log`。
- WebSocket 请求（`Upgrade` 头）走隧道透传；SSE/分块响应逐块流经 `body_filter`。
- 转发时追加 `X-Forwarded-For`（在 `openrusty-proxy::forward` 中合并已有值）。
- 配置路径解析顺序：命令行参数 1 > 环境变量 `OPENRUSTY_CONFIG` > `config/openrusty.toml`。
- SIGHUP 触发异步热重载（不中断服务），见 `reload.rs`。
- 启动/热重载后按 upstream `health.active` 配置运行主动健康探测任务（`active_probe.rs`）；指标在请求路径与转发尝试处记录（`metrics.rs`）。
- 监听面（`listeners.rs`）：`[[server.listeners]]` 按 role 装配并分派 plain/TLS/transparent 三种 accept 循环；空 listeners 从 `server.listen` 派生单 inbound；admin 监听器存在时 `/openrusty/*` 只挂 admin socket。
- 透明拦截（`transparent.rs`）：SO_ORIGINAL_DST → 回环守卫（仅 transparent 监听器；fail-fast，误配拒全部连接）→ 协议探测（H1/H2/Opaque，嗅探前缀经 `PrefixedStream` 回注）→ HTTP 进管线 / opaque 隧道；入向降级、出向关连接。
- egress 三态（`egress.rs`）：direct/gateway/deny 纯决策矩阵 + 处置计数；gateway 明文字节透传，dial 失败/超 3s fail-close。
- ingress 接线（`ingress/`）：每个资源路径一个 watch loop（防抖 200ms），第二防抖窗合并后 render+apply；冲突/渲染错误拒绝整次 apply 保留旧运行时（stale-serve）；成功时按当前插件代数原子发布，不重编译插件。
- TLS（`tls/`）：SNI 动态解析器全 TLS 监听器共享；ingress Secret 命中优先、静态对兜底 miss（两者皆无握手失败）；轮换只影响新握手。
- 生命周期（`shutdown.rs`）：SIGTERM/SIGINT 与 `POST /openrusty/shutdown` 同一信号，三段式（停 accept → 有界 drain，`shutdown_grace_ms` 默认 5000 → 汇总日志）；`/openrusty/ready` draining 时 503、`/openrusty/live` 恒 200。

## 核心链路
1. 请求进入 → `h2c` accept 循环识别 HTTP/1.1 或 h2c，注入 `ConnectInfo`（远端地址）。
2. `app::router` 匹配：管理端点直接应答；否则落入 `pipeline::handle_request`。
3. 匹配业务路由 → 依次运行前置四阶段插件（经 [WASM 插件运行时](../wasm-runtime/index.md)）。
4. 默认代理：`balancer` 阶段选 peer → `openrusty-proxy::forward` 转发（失败重试）。
5. 上游响应头到达 → `header_filter`；响应体逐块 → `body_filter`；整体结束或出错 → `log`。

## 依赖与接口
- 依赖 `openrusty-core`（config/context/Decision）、`openrusty-wasm`（`PluginRegistry` 快照）、`openrusty-proxy`（forward/tunnel/health）。
- 对外接口：监听端口上的全部网关行为 + `GET /openrusty/status` + `POST /openrusty/reload` + `GET /openrusty/metrics` + `GET /openrusty/ready` + `GET /openrusty/live` + `POST /openrusty/shutdown`；一次性子命令 `openrusty iptables-init`、`openrusty inject`。
- 代码锚点：`crates/openrusty-server/src/{main,h2c,app,pipeline,ws,body_filter,reload,active_probe,metrics}.rs`；本批次新增 `{listeners,transparent,egress,shutdown}.rs` 与 `{tls,ingress,init,inject}/` 子模块、`crates/openrusty-k8s/`（被消费库）。

## 关联模块
- [WASM 插件运行时](../wasm-runtime/index.md)
- [代理与负载均衡](../proxy/index.md)
- 业务视角：[features](../../features/index.md)
