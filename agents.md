Commit: 08a95ba
# OpenRusty Overview

## Overview
- OpenRusty 是一个 nginx 风格的 API 网关框架：Rust 网关二进制负责接入、阶段编排与转发，插件是独立的 WebAssembly 模块，运行时从插件目录加载、可热重载，网关与插件完全解耦。
- workspace 包含六个 crate：`openrusty-core`（config、请求上下文、phase 与 Decision 语义）、`openrusty-wasm`（wasmtime 运行时：ABI imports、沙箱 runner、热重载 registry）、`openrusty-proxy`（upstream、负载均衡、被动+主动健康、连接池、转发）、`openrusty-server`（`openrusty` 二进制：同端口 HTTP/1.1+h2c、阶段管线、`/openrusty/*` 端点）、`openrusty-sdk`（`no_std` 插件 SDK）、`openrusty-macros`（`#[phase(...)]` 过程宏）。
- `plugins/` 下是两个各自独立 workspace 的一方插件（`wasm32-unknown-unknown` cdylib）：`vllm-kv-scheduler`（vLLM 风格 KV-cache 亲和调度）与 `kv-probe`（覆盖全部 8 个阶段的 e2e 探针，供集成演练使用）。
- 插件 ABI 契约的权威文档是 [docs/wasm-abi.md](./docs/wasm-abi.md)：8 个 nginx 对齐阶段、`orr_on_phase` 导出、`openrusty` 命名空间 host imports、nginx 对齐的 Decision 返回码。
- 请求的语义骨架：请求进入后依次经过 `post_read`/`rewrite`/`access`/`content` 四个前置阶段，`content` 的默认处理器是代理转发；每次 upstream 尝试前运行 `balancer` 阶段；响应侧依次是 `header_filter`、按块流式的 `body_filter`、收尾的 `log`。
- 配置为 TOML（示例 [config/openrusty.example.toml](./config/openrusty.example.toml)）；生产部署以 systemd 单元运行（`openrusty.service` + `openrusty-echo@.service`），详见 [运维与部署](./features/ops/index.md)。
- 验证分三层：`cargo test --workspace`（单元）、`scripts/build-plugins.sh`（插件单测 + wasm 构建）、`scripts/integration.sh`（e2e 演练，93 checks）。

## Agent 模块索引
- [网关请求管线](./agents/gateway-pipeline/index.md) —— `openrusty-server`：接入、阶段编排、管理端点
- [WASM 插件运行时](./agents/wasm-runtime/index.md) —— `openrusty-wasm`：ABI、沙箱、热重载、host KV
- [代理与负载均衡](./agents/proxy/index.md) —— `openrusty-proxy`：upstream、均衡、健康、转发

## Features 索引
- [features/index.md](./features/index.md)
