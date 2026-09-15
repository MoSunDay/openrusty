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

## 热重启（进程替换，与重载互补）
- systemd socket activation（`openrusty.socket` 持 fd + `Backlog=511`）：`systemctl restart` 时旧进程按 grace 排空在途，新连接进 backlog，新进程经 `LISTEN_FDS` 继承 fd 立即消费——**0 拒连**，在途请求全部完成（演练压载断言 connect_errors=0）。崩溃重启（`Restart=on-failure`）同理，backlog 兜住 SYN。
- `sd_listen.rs` 的 fd 继承模块按可复用设计；**USR2 二进制热升级延后**：socket activation + 优雅排空已交付其核心价值（0 拒连、grace 排空、`systemctl restart` 换二进制），残余价值仅剩 grace 窗口外的长连接存活与非 systemd 部署，触发条件出现再立项（届时 = 继承模块 + exec 自身 + PID 文件 + oldbin 排空协议）。

## 关联逻辑模块
- [WASM 插件运行时](../../agents/wasm-runtime/index.md)
- [网关请求管线](../../agents/gateway-pipeline/index.md)
