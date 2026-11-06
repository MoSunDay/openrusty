# ABI v2 P0：请求改写 imports、一等 Redirect、无路由 log 对齐（192 → 203 checks）

Commit: (working-tree, pre-initial-commit)

## Context
- ABI v2 路线图 P0 三项：① 插件在阶段内改写请求（uri/query/header）并使转发真实生效；② Redirect 成为一等能力（SDK 单调用 + 短路响应不丢头）；③ 无路由 404 与 nginx 对齐（默认仍跑 `log` 阶段）。
- 冻结契约（评审通过）：imports `req_set_uri/req_set_query/req_set_header/req_del_header`（成功 0 / 非法 -1 且零副作用）；SDK `respond_redirect`（非 3xx 收敛 302）；`[plugins] log_on_no_route` 默认 `true`。

## Change Summary
### A. P0.1 请求改写 imports（`openrusty-wasm`/`openrusty-sdk`）
- 新增 `linker_rewrite.rs`（352 行）：四 import + 纯校验函数（`valid_uri`/`valid_query`/`set_header`/`del_header`）。uri/query ≤8KiB；uri 禁 `?#` 与控制符、query 禁前导 `?`；`content-length` 保护（set→-1、del→0 无操作）；头替换大小写不敏感 replace-all。
- 双轨语义：import 立即改写 session ctx（转发侧直接消费 post-phase ctx），同时记录 `ReqEdit`（`instance.rs`；`take_req_edits` 暂无消费者，供未来审计/回放）。
- 网关侧（`pipeline/mod.rs`）：转发取 `session.ctx()` 的 path/query/headers，改写直达 upstream。
### B. P0.2 一等 Redirect
- SDK `host::respond_redirect`：写 30x + `Location` 并以 `Done` 短路；非 3xx 收敛 302。
- 计划外真缺陷修复（`resp_shortcut.rs`）：无 body 的 `Done`/`Deny` 分支此前丢弃插件响应头（302 丢 `Location`、204 丢 `X-Log404`）。`with_plugin_headers` 增 `default_content_type` 参数，deny 统一经 `body_response`，空 204 应用头但不注入默认 content-type；旧测试逐字节保持。
### C. P0.3 无路由 log 对齐（`openrusty-core`/`openrusty-server`）
- `[plugins] log_on_no_route`（默认 `true`）：无匹配路由 404 前仍执行 `log` 阶段插件；`false` 恢复旧行为。实现在 pipeline 早退分支（与简报的 app.rs::fallback 语义等价、更局部，评审认可）。
### D. 验证面与文档
- kv-probe 新探针：`rwuri`/`rwhdr`/`redir`/`l404` 标志与 `mode=l404check`（单测拆至 `plugins/kv-probe/src/tests.rs`）。
- e2e：§30 扩 4 checks + 新增 §98 `98_no_route_log.sh` 8 checks，主 drill **192 → 203**（GATE_PORT=18302 实测 203/203）。
- `docs/wasm-abi.md` 新增 "Request rewriting" 节；`config/openrusty.example.toml` 示例 `log_on_no_route`；README/agents/features 计数同步 203；agents/wasm-runtime、agents/gateway-pipeline 按新事实刷新（imports 23 个、无路由 log、无 body 短路保头）。

## 验证
- `cargo test --workspace`：**597 passed / 0 failed**（`-- --skip phase_signature_validation`；该 UI 测试失败为**预存在环境伪影**：cargo rustc target-info 探针 stdin 与捕获管道竞态毒化 `target/tests/trybuild/.rustc_info.json`，干净 HEAD 5/5 复现、setsid/分离会话均无法规避，CI 不受影响；留证 `/tmp/gate_phase.log`，产品侧不修）。
- `cargo clippy --workspace --all-targets` 与 plugins/kv-probe（`--target wasm32-unknown-unknown`）：工作树 vs HEAD 双树 warning multiset 完全一致，**零新增**。
- `cargo build --workspace` 干净；`cargo fmt --check` 通过；行数合规（新增文件 ≤400、迭代文件 ≤800）。
- 评审遗留（非阻塞）：① `log_on_no_route` 默认开启的 404 洪水容量评估；② `take_req_edits` 消费者决策（消费/设上限/移除）。
