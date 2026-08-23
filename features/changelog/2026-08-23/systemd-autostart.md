Commit: d9a7ede
# systemd 开机自启部署落地（115b8fe）

## Context
- 此前网关与演示上游只能手动 `cargo run` / 后台启动，重启机器后服务不回来，也没有统一的日志与启停入口；需要一个可长期运行的部署形态。

## Change Summary
- 新增 `scripts/install-service.sh`：幂等安装脚本，写入并启用两个 systemd 单元。
- 新增 `config/openrusty.toml`：正式运行配置（网关监听 `127.0.0.1:8180`，插件目录 `build/plugins`，upstream `vllm` 指向三个 echo peer）。
- `openrusty.service`：网关单元，`WorkingDirectory=/root/openrusty`（配置使用相对插件目录），`ExecStart` 加载 `config/openrusty.toml`，`Wants`/`After` 三个 echo 实例。
- `openrusty-echo@.service`：演示 echo 上游模板单元（`%i` = 端口），启用实例 `9001`/`9002`/`9003`。

## Impact Surface
- 运维模型：网关与上游随开机自启，全部 enabled + active；日志统一到 journald（`journalctl -u openrusty`）。
- 脚本可重复执行：重写单元文件并 restart 全部单元，新构建的 release 二进制即刻生效。
- [运维与部署](../../ops/index.md) 能力由此成立。

## Notes / Compatibility
- 安装需 root，且要求先构建 `target/release/openrusty` 与 `echo_upstream` example，否则脚本快速失败。
- 停用方式：`systemctl disable --now openrusty.service`（echo 实例同理）。
- `WorkingDirectory` 不可省略，否则相对路径 `build/plugins` 解析失败。

## Related Docs
- [运维与部署](../../ops/index.md)
- [热重载](../../hot-reload/index.md)
- [网关请求管线](../../../agents/gateway-pipeline/index.md)
