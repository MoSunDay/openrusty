Commit: 08a95ba
# 主动健康探测启动竞态修复：首个成功探测不再静默标脏全部 peer

## Context
- 集成演练间歇性失败（~60-80% 运行中 `stable after repeat (t2)`、`affinity created` 两项挂掉）：启动后约 1 个探测周期内（500ms）所有 peer 被静默标脏，窗口内的请求全部 502，随后 "healthy again" 恢复。
- 根因在 `evaluate_active` 纯函数：成功路径返回 `successes >= healthy_threshold`，全新 peer 的**第一次成功探测**（successes=1 < 2）把 `active_ok` 写成 false——阈值本应只门控"从脏恢复"，却把"尚未证明恢复"当成"不健康"。失败路径对称缺陷：完全无视当前健康位，脏 peer 在「一次成功重置计数 → 一次失败」后被翻回 healthy（fails=1 < 2）。
- 诊断难点：标脏发生在首观测（`transition(None, false)` 不记日志），网关日志只留恢复痕迹。

## Change Summary
- `evaluate_active` 增加 `healthy: bool` 入参：成功路径 `healthy || successes >= healthy_threshold`（成功永不把健康 peer 变脏），失败路径 `healthy && !(fails >= unhealthy_threshold)`（失败永不治愈脏 peer）。
- `record_probe` 传入 `peer.healthy` 当前值；模块与函数文档同步。
- `active_probe.rs` 探测失败路径新增 WARN 日志（原因：timeout / 转发错误 / 非 2xx 状态码），消除"静默标脏"的可观测性盲区。

## Impact Surface
- 语义：`active_ok` 初始 true（fail-open）与新逻辑一致——启动窗口不再有全 peer 标脏竞态；阈值语义回归 lua-resty-upstream-healthcheck 原意（门控状态迁移而非绝对判定）。
- 单测：`evaluate_active_pure_semantics` 断言按修正语义重写并新增 boot-race 回归断言；`active_success_resets_fail_counter` 修正（旧断言编码了 bug 行为）。
- 验证：工作区 12 个测试目标全绿；演练 93/93 连跑 5 次稳定（修复前同条件多数失败）。

## Notes / Compatibility
- 无配置、无 ABI 变更；纯状态机语义修正。
- 探测失败 WARN 每次失败一条（受 interval 限频），kill 类演练段落会按预期出现 Connection refused 记录。

## Related Docs
- [代理与负载均衡](../../agents/proxy/index.md)
- [运维与部署](../ops/index.md)
