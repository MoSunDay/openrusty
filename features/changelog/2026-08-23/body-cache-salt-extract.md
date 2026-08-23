Commit: 7ef1e9f
# 亲和键改为请求体 cache_salt 提取（vLLM 风格，替代 ?task= 查询参数）

## Context
- 真实客户端（如 /root/opencoder）按 vLLM 约定把 `cache_salt`（值 `<agent>:<session_id>`）作为 JSON 请求体**顶层字段**发送，用于 KV/prefix cache 命名空间化；网关原先只支持从 URL（`query:`/`path:`）提取亲和键，客户端必须额外挂 `?task=`，不符合该契约。

## Change Summary
- ABI：`req_meta` 新增 `"body"` 键，返回已缓冲的完整请求体（≤16MiB）；管线在 content 阶段前缓冲请求体，故 content/balancer/log 可用，post_read/rewrite/access 为空（`docs/wasm-abi.md` 已载明）。
- 数据通路：`HostData.req_body` + `RequestSession::set_req_body`，pipeline 缓冲后灌入会话；SDK 增加 `host::req_body()` 包装。
- kv-scheduler：`extract` 新增 `body:<json-field>` 变体，no_std 手写最小 JSON 扫描器 `json_string_field`（精确字段名、转义还原、`\u` BMP 解码、首现匹配、非字符串值跳过）；trim 后为空视为缺失 → 不参与调度。
- 生产配置 `config/openrusty.toml`：`extract` 由 `query:task` 切换为 `body:cache_salt`（重启网关生效）。
- 演练新增 §25（salt 粘滞 / 分散 / 无 salt 回退），67 → 70 checks。

## Impact Surface
- 调度语义：同 `cache_salt` 粘滞同实例（同会话的 slot/prefix cache 得以复用）、异 salt 分散、无 salt 走默认均衡——经 node03 真实集群验证（`/tokenize` 探针 + 真实生成，落点以日志 `peer=` 为准）。
- 请求体经 req_meta 对全部插件可见，属 ABI 能力扩展（只增不改，旧插件不受影响）。
- 三层验证：111 工作区测试、插件 13 测试、演练 70/70（连跑 5 次稳定）。

## Notes / Compatibility
- `json_string_field` 不感知嵌套：字段名在嵌套对象/字符串中首现即命中（已在注释与测试中固化该语义）。
- 请求体上限 16MiB（超限 413）；扫描代价与体长线性相关，插件阶段超时仍为默认值。
- `/root/opencoder` 侧是发送端实现（出站注入 `cache_salt`），本次为网关侧的提取端落地。

## Related Docs
- [kv-scheduler 亲和调度](../../vllm-kv-scheduler/index.md)
- [WASM ABI 契约](../../../docs/wasm-abi.md)
- [运维与部署](../../ops/index.md)
