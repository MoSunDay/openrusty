Commit: abe419f
# Upstream TLS（https 上游）+ 嵌入 API 正式化（M2 + M1）

## Context
- service mesh 路线（k3as 数据面内嵌 openrusty）的两个硬缺口：① 网关到上游只有明文 HTTP，mTLS 前置不可行；② k3as 复用的 listener/嵌入面（`serve_listener`、`ConnTimeouts`、`ProtoMode` 等）卡在 `pub(crate)`。本条目补齐 ①（M2 全量）并放开 ②（M1 openrusty 侧；k3as 侧接线由 T5.5 相邻项记账）。

## Change Summary
### A. 配置面（`openrusty-core/src/config/tls.rs`）
- `[[upstreams]]` 新增可选 `tls` 节：`server_name`（必填，SNI+校验名）、`ca_cert`（信任锚 PEM；与 `insecure_skip_verify` 二选一）、`client_cert`/`client_key`（mTLS 对，二者同进同退）、`insecure_skip_verify`（默认 false）。校验错误在 `load_config` 阶段报出。
### B. 代理面（`openrusty-proxy/src/tls/`、`client.rs`、`forward.rs`、`upstream.rs`）
- `tls/mod.rs`：`build(UpstreamTlsConfig) -> UpstreamTls`（rustls 配置 + 内容寻址 `TlsClientKey`）；`tls/connector.rs`：hyper legacy 连接器，**直拨 peer 地址**（不走 DNS/HttpConnector），SNI 取 `server_name`，ALPN 仅 `http/1.1`。
- `ClientPool` 键升级为 `PoolKey::{Http,Https}`：https 客户端按 `(addr, TlsClientKey)` 池化，热重载同配置复用连接；`evict_except` 按键驱逐。
- `forward_peer` 统一两个 scheme 的重试循环（`pipeline`/`active_probe` 共用）；TLS 握手/建连失败归类 `ForwardError::Connect`（可重试）；Host 头保持 peer 地址（nginx `proxy_pass` 语义）。
### C. 服务面（`openrusty-server`）
- `state.rs build_tls_plans` 在 **reload 发布前**构建全部证书材料：失败 = reload 拒绝（旧 plugins+runtime 原样保留），boot 期 fail-fast；`apply_runtime` 只消费预构建结果（保持不可失败）。ingress 渲染路径同规则（warn + 保旧）。
- `pipeline`/`ws`/`active_probe` 按 upstream 的 tls 有无选择客户端与 URI scheme。
### D. M1：嵌入面可见性
- `h2c::{serve_listener, ConnTimeouts, conn_timeouts, ProtoMode}`、`transparent::EgressPlane`、`tls::TlsPlan`、`listeners::Mount` 字段 → `pub`；内部接线（`serve_conn`、各 `serve_listener` 内部实现）保持 `pub(crate)`。`RequestSession::for_parts` 允许嵌入方共享 registry 的 engine/linker。

## 测试覆盖
| 面 | 载体 |
|----|------|
| 配置解析/校验（缺 ca、client 对不齐） | `openrusty-core` config 单测 |
| TLS 构建（缺文件拒绝、insecure 通过、键等值） | `openrusty-proxy/src/tls` 单测 |
| 池化语义（http/https 同址共存、按键驱逐） | `openrusty-proxy/src/client.rs` 单测 |
| 转发（两 scheme 共享重试循环） | `openrusty-proxy`/`openrusty-server` 单测 |
| e2e | `scripts/integration.sh` §32（CA 校验 200、错 CA 502、insecure 200） |

## Impact Surface
- 默认零变化：无 `tls` 节的 upstream 行为与性能不变；新增依赖 rustls/tokio-rustls（workspace 既有，ring provider）。
- 运营注意：`insecure_skip_verify` 仅应急/演练；`ca_cert` 文件在 reload 时重读（与静态监听证书不同），文件丢失 = reload 拒绝而非静默降级。

## Related Docs
- [代理与负载均衡](../../agents/proxy/index.md)（出向 TLS 设计段）
- [运维与部署](../ops/index.md)（验证计数 119 checks）
- `config/openrusty.example.toml`（注释版 `[upstreams.tls]` 块）
