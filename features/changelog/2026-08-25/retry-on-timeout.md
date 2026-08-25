Commit: 08a95ba
# 路由超时可选换 peer 重试（retry_on_timeout）

## Context
- 路由整请求超时原为终态 502：不记失败、不换 peer。上游单 peer 偶发 hang 时，明明有健康 peer 可用也直接失败。
- 推理类 POST 重放有双倍计算成本，因此重试必须显式 opt-in。

## Change Summary
- `crates/openrusty-core/src/config.rs`：upstream 新增 `retry_on_timeout`（默认 `false`）。
- `crates/openrusty-proxy/src/upstream.rs`：`from_config` 映射该字段。
- 转发路径（`openrusty-proxy`/`openrusty-server` pipeline）：`retry_on_timeout = true` 时，路由超时在当前 peer 记一次失败并换下一个健康 peer 重来，仍受 upstream `retries` 上限约束；默认行为不变（超时即 502）。
- `config/openrusty.example.toml`：注释说明。
- `scripts/integration.sh` §27：确定性 hang peer 上验证默认不重试 vs 开启后换 peer 成功。

## Impact Surface
- 配置：新增可选布尔字段，缺省语义与旧版完全一致。
- 不影响：连接级失败重试语义、ABI、插件。

## Related Docs
- [代理转发](../../proxying/index.md)
- [负载均衡与健康检查](../../load-balancing/index.md)
