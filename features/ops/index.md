Commit: 681ad49
# 运维与部署

## 能力概述
- 网关自带管理端点用于观测与触发重载；提供幂等的 systemd 安装脚本，实现网关与演示上游的开机自启；配套三层验证手段。

## 触发方式
- 观测：`GET /openrusty/status`（状态 JSON）与 `GET /openrusty/metrics`（Prometheus 指标）。
- 部署：`sudo bash scripts/install-service.sh`（需先构建 `target/release/openrusty` 与 `--example echo_upstream`）。
- 日志：`journalctl -u openrusty`；停用：`systemctl disable --now openrusty.service`。

## 行为与规则
- `/openrusty/status` 返回 JSON：`generation`、`uptime_secs`、`plugins`（每插件错误计数）、`routes` 数、`upstreams`（每 upstream 的 `peers`/`healthy`）。
- `/openrusty/metrics` 返回 Prometheus 文本暴露：`openrusty_requests_total`（route/code）、`openrusty_request_duration_seconds` 直方图、`openrusty_upstream_attempts_total`（upstream/result）、`openrusty_plugin_errors_total`、`openrusty_peer_healthy` gauge、`openrusty_kv_entries` gauge。
- 重载触发见 [热重载](../hot-reload/index.md)。
- systemd 单元：
  - `openrusty.service` —— 网关，`WorkingDirectory=/root/openrusty`（配置引用相对插件目录 `build/plugins`），`ExecStart` 读 `config/openrusty.toml`，`Wants`/`After` 三个 echo 实例，`LimitNOFILE=65535`；
  - `openrusty-echo@.service` —— 模板单元，`%i` 为端口/节点号，实例 `9001`/`9002`/`9003` 作为演示上游。
  - 脚本幂等：重复执行会重写单元文件并 restart 全部单元，从而加载新构建的二进制；所有单元 enabled + active，开机自启。
- 生产上游拓扑：`config/openrusty.toml` 的 upstream `vllm` 指向 node03 llama-server 集群（`192.168.31.224:9001-9003`，一卡一实例 ×3，Qwen3.8-27B-UD-Q4_K_M，`-c 150000 --no-kv-offload`（KV cache 放系统内存，约 5.3GB/实例）、q8_0 KV、MTP 投机解码（`--spec-type draft-mtp`，约 727MiB 显存/卡）），由 node03 上的模板单元 `llama-server@<gpu>:<port>.service` 管理；路由超时 120000ms（V100 生成较慢）。本地 echo 实例仅保留演示用途，生产配置不再引用。
- 配置加载顺序：命令行参数 1 > 环境变量 `OPENRUSTY_CONFIG` > `config/openrusty.toml`；示例见 `config/openrusty.example.toml`。
- 三层验证：`cargo test --workspace`（单元）；`scripts/build-plugins.sh`（插件单测 + wasm 构建）；`scripts/integration.sh`（e2e 演练，119 checks，覆盖代理/流式协议、调度、热重载、被动+主动健康、retry_on_timeout、收容、路由超时、kv-probe 全阶段、status/metrics 形状、env 配置启动、并发 reload 竞争、动态 WASM 执行 API、upstream TLS 等）。

## 上线红线（部署前必读）
- 插件故障策略 `fail_open` 为默认放行 —— 插件崩溃/超时时请求仍被放行，安全语义依赖 access 阶段显式 `fail_close`。
- `/openrusty/*` 管理端点无鉴权 —— 必须仅绑定内网/回环，或由外部防火墙保护。
- 静态 TLS 证书在 reload 时不会重读 —— 证书轮换需重启网关（SNI 动态证书除外）。
- upstream TLS 材料按证书文件路径寻址 —— 同路径换发新证书后，reload 沿用旧连接池（旧证书继续生效）；轮换 upstream 证书需更换文件路径再 reload，或重启网关。
- 请求体最大 16MiB 且全程缓冲在内存 —— 大文件场景不适用，需另行限流/隔离部署。

## 关键状态与异常
- 状态：各单元 enabled/active；`/openrusty/status` 的 `generation` 与插件错误计数。
- 上游健康：`curl http://192.168.31.224:900x/health` 应返回 `{"status":"ok"}`；`journalctl -u openrusty` 访问日志的 `peer=` 字段可核对请求真实落点（访问日志需 `log_level = "info"`，2026-08-26 起默认 `warn`）。
- 异常：缺少 release 二进制或 `config/openrusty.toml` 时安装脚本直接报错退出；非 root 执行被拒绝。

## 关联逻辑模块
- [网关请求管线](../../agents/gateway-pipeline/index.md)
