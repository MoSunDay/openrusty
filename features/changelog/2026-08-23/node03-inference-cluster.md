Commit: 2bf8269
# node03 三卡推理集群接入（替代本地 echo 上游）

## Context
- 此前生产 upstream `vllm` 指向本地 9001–9003 三个 echo 演示实例；kv-scheduler 的 task 亲和调度需要接真实 LLM 推理才能端到端验证，且 node03 三张 32GB 卡被单个 Q8_K_XL + 450k ctx 实例（:8080，横跨三卡）占满。

## Change Summary
- node03（192.168.31.224）：停掉并禁用旧 `llama-qwen38-27b.service`；改为每卡一个 llama-server 实例 ×3（:9001/:9002/:9003），模型 Qwen3.8-27B-UD-Q4_K_M（自 node04 拷入 `/opt/models/`），模板单元 `llama-server@<gpu>:<port>.service` + `/opt/llama/run.sh`（`CUDA_VISIBLE_DEVICES` 锁单卡），参数 `-ngl 99 -c 16384 --parallel 2 -ctk q8_0 -ctv q8_0 -t 8`，全部 enabled + active。
- `config/openrusty.toml`：upstream `vllm` peers 改为 `192.168.31.224:9001-9003`，路由 `timeout_ms` 30000 → 120000；`POST /openrusty/reload` 热生效（未重启网关）。

## Impact Surface
- kv-scheduler 亲和调度经真实推理验证通过：同 `task=` 粘滞同实例、异 task 分散（以 `journalctl -u openrusty` 日志 `peer=` 字段为准）；真实 chat completion 成功（prompt ~99 tok/s）。
- 每卡显存 ~16.4GB（16GB 权重 + 16k ctx 的 q8_0 KV），余量充足；`/openrusty/status` healthy=3。
- 本地 echo 实例与 `scripts/integration.sh` 不动（演练用独立端口与自建配置），回归仍 67/67。

## Notes / Compatibility
- node03 侧文件（`/opt/llama/run.sh`、`/etc/systemd/system/llama-server@.service`、模型）在远端机器上，不在本仓库。
- Qwen3.8 为 reasoning 模型：`max_tokens` 偏小时 token 会全部耗在 `reasoning_content` 而 `content` 为空，属模型行为而非网关问题。
- node03 集群不可用时，把 peers 改回 `127.0.0.1:9001-3` 并 reload 即回退 echo 演示。

## Related Docs
- [运维与部署](../../ops/index.md)
- [kv-scheduler 亲和调度](../../vllm-kv-scheduler/index.md)
