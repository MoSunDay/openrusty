Commit: 08a95ba
# 网关请求管线（openrusty-server）

## 职责
- `openrusty` 二进制：同端口 HTTP/1.1 + h2c 接入、请求路由、8 阶段插件管线编排、WebSocket/SSE 透传、管理端点。

## 边界
- 负责：连接接入与协议识别（`h2c.rs`）；axum 路由（`app.rs`：`/openrusty/*` 优先，其余落入代理管线）；阶段顺序编排与 Decision 处置（`pipeline.rs`）；响应流式处理（`body_filter.rs`）；WebSocket 升级透传（`ws.rs`）；配置路径解析与进程启动（`main.rs`）。
- 不负责：插件的执行与沙箱语义（[WASM 插件运行时](../wasm-runtime/index.md)）；peer 选择、健康与转发细节（[代理与负载均衡](../proxy/index.md)）；配置结构与 Decision 定义（`openrusty-core`）。

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

## 核心链路
1. 请求进入 → `h2c` accept 循环识别 HTTP/1.1 或 h2c，注入 `ConnectInfo`（远端地址）。
2. `app::router` 匹配：管理端点直接应答；否则落入 `pipeline::handle_request`。
3. 匹配业务路由 → 依次运行前置四阶段插件（经 [WASM 插件运行时](../wasm-runtime/index.md)）。
4. 默认代理：`balancer` 阶段选 peer → `openrusty-proxy::forward` 转发（失败重试）。
5. 上游响应头到达 → `header_filter`；响应体逐块 → `body_filter`；整体结束或出错 → `log`。

## 依赖与接口
- 依赖 `openrusty-core`（config/context/Decision）、`openrusty-wasm`（`PluginRegistry` 快照）、`openrusty-proxy`（forward/tunnel/health）。
- 对外接口：监听端口上的全部网关行为 + `GET /openrusty/status` + `POST /openrusty/reload` + `GET /openrusty/metrics`。
- 代码锚点：`crates/openrusty-server/src/{main,h2c,app,pipeline,ws,body_filter,reload,active_probe,metrics}.rs`。

## 关联模块
- [WASM 插件运行时](../wasm-runtime/index.md)
- [代理与负载均衡](../proxy/index.md)
- 业务视角：[features](../../features/index.md)
