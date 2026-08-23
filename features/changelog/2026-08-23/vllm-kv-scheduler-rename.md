Commit: 74e5987
# kv-scheduler 更名为 vllm-kv-scheduler（框架前缀命名约定）

## Context
- 未来可能为其他推理框架（非 vLLM 系）提供同构的 KV 亲和调度插件，需要按框架区分命名。

## Change Summary
- 纯重命名：`plugins/kv-scheduler` → `plugins/vllm-kv-scheduler`（目录、包名、产物 `vllm-kv-scheduler.wasm`、配置 `order`/`settings` 段、日志前缀、文档与链接同步）；`features/kv-scheduler/` 同步迁移。
- 确立命名约定 `<推理框架>-kv-scheduler`（写入 [vllm-kv-scheduler 亲和调度](../../vllm-kv-scheduler/index.md)）。
- `scripts/build-plugins.sh` 增加通用逻辑：构建前清理 `build/plugins/` 中与现有插件目录不对应的残留 `.wasm`（防止旧产物被 registry 顺带加载）。

## Impact Surface
- 生产网关经 `POST /openrusty/reload` 热切换至新插件（无需重启）；亲和记录键（`aff:*`/`sched:*`）不含插件名，调度状态不受影响。
- 历史正文中的旧名保留（`features/changelog/**`），仅修正失效链接。

## Related Docs
- [vllm-kv-scheduler 亲和调度](../../vllm-kv-scheduler/index.md)
