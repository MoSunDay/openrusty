Commit: 681ad49
# e2e 覆盖盲区闭环：174 → 192 checks + CI k8s-e2e 门禁

## Context
- 今日只读审计 [e2e-coverage-audit.md](./e2e-coverage-audit.md) 逐项 grep 证实了 8 个零覆盖盲区，并给出三条落地建议（补断言、新增小节、CI 挂入环境门控脚本）。本次全部执行：主 drill **174 → 192（+18）**，连续两轮 192/192 全绿。
- +18 分布：§96 配置面 8 / §88 优雅排空 3 / §97 管线 `resp_body_set` 3 / §29 指标族 2 / §1 XFF 合并 1 / §18 `uptime_secs` 1。

## Change Summary
### A. 新增三个 drill 小节（+14）
- §88 优雅排空（`scripts/integration/88_graceful_drain.sh`，3 checks）：独立网关（`shutdown_grace_ms = 8000`）+ 只 accept 不应答的上游。探针是**纯 stdlib 的裸 h2c 客户端**：park 一个 `/hang` 流保住在途连接，收到首个 GOAWAY 后、应答 shutdown ping 前打开 `/openrusty/ready`（流 3）与 `/openrusty/live`（流 5）——断言 ready 回 503 `{"status":"draining"}`、live 保持 200、进程等满 grace 后 exit 0（与 §85 SIGQUIT 跳过排空形成对照）。
- §96 配置面（`scripts/integration/96_config_surface.sh`，8 checks，独立端口 + 独立 TOML）：① 不等权 swrr（3:1，40 请求 = 10 个完整权重和周期）精确 30/10；② 静态路由 `host`+`exact`：Host 与 path 双命中 → upstream A，同 path 换 Host 落 catch-all B，`exact` 绝不前缀匹配（`/hit/extra` → B）；③ upstream mTLS：带 `client_cert/client_key` 转发成功 200，同 server 缺客户端证书 → 握手失败、网关自有 502；④ `http1_only`：明文 HTTP/1.1 正常应答 1.1，h2c prior-knowledge preface 被拒。
- §97 管线 `resp_body_set`（`scripts/integration/97_resp_body_set.sh`，3 checks）：经 kv-probe 新探针模式闭环"写 body + 终态 Decision"短路语义，此前仅 dynamic API 路径（`lib-dynamic-drill.sh`）覆盖。
### B. 存量小节补断言（+4）
- §1（`10_proxy_basics.sh`）：带已有 `X-Forwarded-For: 203.0.113.9` 请求 → 合并为 `203.0.113.9, 127.0.0.1`（补齐 `proxy::merge_xff` 的 append 语义，与 `crates/openrusty-proxy/src/forward.rs` 的 `merge_xff_appends_client_ip` 单测对齐）。
- §18（`30_plugins_probe.sh`）：`/openrusty/status` 的 `uptime_secs` 为非负整数（`app.rs` 渲染 `elapsed().as_secs()`，永不为浮点）。
- §29（`50_retry_metrics_race.sh`）：`# TYPE openrusty_plugin_phase_seconds histogram` 指标族存在 + `openrusty_plugin_phase_seconds_count{phase="log"}` 已有观测（log 阶段对每个已代理请求都跑过）。
### C. kv-probe 新探针模式 `mode=resp-body`
- `plugins/kv-probe/src/lib.rs` 新增 `ProbeMode::RespBody` 与纯函数 `resp_body_text` / `deny_decision`（wire 格式与 Decision 映射均被单测钉住）。契约表：

| query | 插件动作 | Decision | 期望响应 |
|---|---|---|---|
| `mode=resp-body&text=hello` | `resp_body_set("kv-probe-resp-body:hello")` | `Done` | 200 + 精确 body |
| `mode=resp-body&text=hello&deny=403` | 同上 | `Deny(403)` | 403 + 同一 body |
| `mode=resp-body`（无 `text`） | 不写 body | `Done` | 204 空 body |
| `deny` 非法/越界（99/600/非数字） | — | `Deny(400)` | 400（仅单测钉住，drill 不跑） |
| `resp_body_set` 被宿主拒绝 | 写失败 | `Deny(500)` | 500（仅单测钉住，drill 不跑） |

- 语义依据 [docs/wasm-abi.md](../../../docs/wasm-abi.md) 的 Decision 映射：`Done`+body → 200+body，`Done` 无 body → 空 204，`Deny(s)` → s（有 body 则随行）。本次确认管线侧与 dynamic API 路径行为一致。
### D. CI 新增 `k8s-e2e` job（`.github/workflows/ci.yml`）
- 把 4 个环境门控演练挂入 CI（此前仅本地手工）：`cluster-e2e.sh`（自带 kubeconfig 缺失 → SKIP + exit 0）、`sidecar-e2e.sh`、`local-netns-test.sh`、`local-egress-test.sh`。
- **sidecar 的 readyz 门控理由**：它没有自跳过路径——S1 把"apiserver 不可达"计为 FAIL（exit 1 + 数分钟无效重试），因此在 CI 里先用与 S1 相同的 `kubectl get --raw=/readyz` 探测决定是否真跑；`bash -n` 语法检查则始终执行。
- netns/egress 直接裸跑：`env_gates` 的 skip() → exit 0，SKIP 中性、真实 FAIL 才挂 job。job 无 toolchain 步骤（所有 cargo 调用都在门控之后）。托管 runner 上四个步骤全部 SKIP，等于零成本的门禁/bash 语法防腐层。
### E. `scripts/lib-netns.sh` 修复
- `netns_teardown` 原以 `[ -n "$TMP" ] && rm -rf "$TMP"` 收尾：SKIP 路径在 `netns_up` 设置 `TMP` 之前就 bail，该表达式返回非零，而演练的 EXIT-trap 以它收尾 → 其 rc 成为脚本退出码，SKIP 变成失败。补 `|| true`，使 SKIP 严格 exit 0（D 裸跑的前提）。
### F. 文档计数修正
- `README.md`（119 checks / sections 1-32 → 192 checks / §1-§34 + §70-§97，并补全 walkthrough 尾部的新增覆盖）、`agents.md` 与 `features/ops/index.md`（174 → 192，覆盖清单补新面 + 记 `k8s-e2e` job）。

## 验证
- `scripts/integration.sh` 连续两轮 **192/192**（静态核对 `check "` 调用数 = 192）；`plugins/kv-probe` 单测 14 通过（新增 `resp-body` 面：mode 解析、wire 格式、`deny` 映射、host-stub Decision、ABI 返回码）；`cargo test --workspace` 582 通过。
- CI `k8s-e2e` 在无 k8s/netns 环境的 runner 上应为 4 个 SKIP 步骤 + 退出码 0。

## Impact Surface
- 主 drill 网关配置零变化：§96/§88 用独立端口（18220-18225 / 18230-18231）与 `$TMP` 下的独立 TOML，不污染既有小节的调度/健康断言（教训见 [integration-port-contamination.md](./integration-port-contamination.md)）。
- kv-probe 新模式默认不触发（仅 `mode=resp-body` 显式命中），既有探针路径与 wasm 体积影响可忽略。
- 审计中的次要盲区仍未做（16MiB 上限、admin listener 独占负向、reload 非 loopback 403、KV 超限拒绝、`instance_pool_size`/`pool_idle_timeout_ms` 等），见审计"次要"清单。

## Related Docs
- 动机：[e2e-coverage-audit.md](./e2e-coverage-audit.md)（只读审计，含全部盲区清单）
- [features/ops/index.md](../../ops/index.md)（192 checks + CI 门禁 + k8s-e2e）、`README.md`、[agents.md](../../../agents.md)
- [docs/wasm-abi.md](../../../docs/wasm-abi.md)（`resp_body_set` 与 Decision 映射）、[features/k8s-forms/index.md](../../k8s-forms/index.md)（netns/egress 演练）
