Commit: d9a7ede
# 热重载

## 能力概述
- 在不重启进程、不中断在途请求的前提下，重新加载配置与全部插件；失败时整体回退、旧版本继续服务。

## 触发方式
- `SIGHUP` 信号：`kill -HUP $(pidof openrusty)`。
- `POST /openrusty/reload`：仅限 loopback 来源，非回环地址返回 403。

## 行为与规则
- 重载流程：重新读取并校验配置 → 后台编译并校验插件目录内全部 `.wasm` → 原子发布新快照，`generation` 递增。
- 原子性：任一步失败则整体拒绝，旧快照原样保留；成功返回新的 `generation` 与插件清单。
- 在途请求用旧快照跑完；新请求使用新快照。
- 状态保留：插件 KV 与上游健康状态按名键控，跨重载保留，不会被重置。

## 关键状态与异常
- 状态：`generation`（当前快照代号，`/openrusty/status` 与重载响应均可见）。
- 异常：配置非法或插件编译/校验失败 → 500 并携带错误，服务不中断；非回环访问重载端点 → 403。
- 负载下重载不产生 5xx（由集成演练持续验证）。

## 关联逻辑模块
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)
- [网关请求管线](../../agents/gateway-pipeline/index.md)
