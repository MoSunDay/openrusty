Commit: d9a7ede
# 运维与部署

## 能力概述
- 网关自带管理端点用于观测与触发重载；提供幂等的 systemd 安装脚本，实现网关与演示上游的开机自启；配套三层验证手段。

## 触发方式
- 观测：`GET /openrusty/status`。
- 部署：`sudo bash scripts/install-service.sh`（需先构建 `target/release/openrusty` 与 `--example echo_upstream`）。
- 日志：`journalctl -u openrusty`；停用：`systemctl disable --now openrusty.service`。

## 行为与规则
- `/openrusty/status` 返回 JSON：`generation`、`uptime_secs`、`plugins`（每插件错误计数）、`routes` 数、`upstreams`（每 upstream 的 `peers`/`healthy`）。
- 重载触发见 [热重载](../hot-reload/index.md)。
- systemd 单元：
  - `openrusty.service` —— 网关，`WorkingDirectory=/root/openrusty`（配置引用相对插件目录 `build/plugins`），`ExecStart` 读 `config/openrusty.toml`，`Wants`/`After` 三个 echo 实例；
  - `openrusty-echo@.service` —— 模板单元，`%i` 为端口/节点号，实例 `9001`/`9002`/`9003` 作为演示上游。
  - 脚本幂等：重复执行会重写单元文件并 restart 全部单元，从而加载新构建的二进制；所有单元 enabled + active，开机自启。
- 配置加载顺序：命令行参数 1 > 环境变量 `OPENRUSTY_CONFIG` > `config/openrusty.toml`；示例见 `config/openrusty.example.toml`。
- 三层验证：`cargo test --workspace`（单元）；`scripts/build-plugins.sh`（插件单测 + wasm 构建）；`scripts/integration.sh`（e2e 演练，67 checks，覆盖代理/流式协议、调度、热重载、健康检查、收容、路由超时、kv-probe 全阶段、status 形状、env 配置启动等）。

## 关键状态与异常
- 状态：各单元 enabled/active；`/openrusty/status` 的 `generation` 与插件错误计数。
- 异常：缺少 release 二进制或 `config/openrusty.toml` 时安装脚本直接报错退出；非 root 执行被拒绝。

## 关联逻辑模块
- [网关请求管线](../../agents/gateway-pipeline/index.md)
