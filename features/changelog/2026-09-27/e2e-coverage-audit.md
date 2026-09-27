Commit: 681ad49
# e2e 覆盖审计（只读 review，代码零改动）

## Context
- 对照 features 文档、`config/openrusty.example.toml`、`docs/wasm-abi.md` 与 admin 端点全集，逐项交叉验证 e2e 演练的覆盖完整性；所有"零覆盖"结论均经 grep 证实，非推断。

## 覆盖快照（commit 681ad49）
- `scripts/integration.sh` 实际 174 条断言（稳定文档原写 152 已过时，本次已修正 `agents.md` 与 `features/ops/index.md`）。分布：00:3 / 10:13 / 20:28 / 30:29 / 40:10 / 50:22 / 60:4 / lib-dynamic-drill:35 / 70:5 / 80:5 / 85:3 / 90:13 / 95:4。
- CI（`.github/workflows/ci.yml`）只跑 `scripts/integration.sh`；`cluster-e2e.sh` / `sidecar-e2e.sh` / `local-netns-test.sh` / `local-egress-test.sh` 全部本地手工执行（各自带无环境 SKIP+exit 0 语义，挂入 CI 零成本）。
- k8s 三形态 e2e 拼图完整：sidecar（sidecar-e2e 集群级 + local-netns 本地级）、ingress watch+TLS（cluster-e2e）、egress 三态（local-egress）。`bench.sh` 是性能测量，无 PASS/FAIL 门禁，不算 e2e。

## 盲区清单（grep 证实零覆盖，按回归风险排序）
1. **代理管线内 `resp_body_set` 短路语义**（Done+body→200、Deny(s)+body、204→200 late-write 升级）——仅 dynamic API 路径覆盖（`lib-dynamic-drill.sh`），管线插件零使用；建议给 kv-probe 加 body 探针模式闭环。
2. **`openrusty_plugin_phase_seconds{phase}` 指标族**——性能优化唯一可观测产物，§29 metrics 断言未覆盖。
3. **upstream mTLS（`client_cert/client_key`）**——§32 只测 ca_cert/错 CA/insecure，"mesh mTLS 出向底座"未闭环。
4. **静态路由 `host` 约束 / `exact = true`**——仅 cluster-e2e 经 Ingress 渲染间接覆盖，静态 `[[routes]]` 配置面零测试。
5. **不等权 `weight` 的 swrr 分配**——所有演练 peer 权重全为 1。
6. **`/openrusty/ready` draining 503 分支**——所有 ready 断言都是 200 方向。
7. **XFF 合并语义**——只断言注入，未带已有 XFF 头验证 append。
8. **`http1_only` / `instance_pool_size` / `pool_idle_timeout_ms` 配置键**——从未出现在任何 e2e 配置；`pool_idle_timeout_ms` 连 example.toml 都缺失（文档-配置漂移，键在 `config.rs`）。

次要：无匹配路由 404（演练网关有 `/` catch-all）、admin listener 独占性负向断言（数据端口应无 `/openrusty/*`）、reload 非回环 403、代理面 16MiB 请求体上限、亲和节点被健康摘除后重选、KV 超限拒绝（>64KiB 值 / 1MiB 总量）、`[dynamic] on_failure=fail_closed`、`[admin] token`×`[dynamic]` 特性交叉、主动健康探测走 TLS 上游、`-t` 输出清单内容校验、静态 `tls_cert/tls_key` 监听证书对与 SNI miss 兜底。

## 后续建议（未执行）
- 低成本补断言：plugin_phase_seconds 指标、status `uptime_secs`、带已有 XFF 的合并、shutdown 期间探 `/openrusty/ready` 503。
- 新增小节：weight 加权、host/exact 静态路由、mTLS peer（复用 §32 TLS 基建）、http1_only。
- CI 挂入 4 个自带 SKIP 的集群/netns 脚本。
