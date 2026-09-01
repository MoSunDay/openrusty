Commit: d9a7ede
# Features Index

## 能力分组
- [代理转发与流式协议](./proxying/index.md) —— 同端口 HTTP/1.1+h2c、WebSocket 透传、SSE、路由超时、连接失败换 peer 重试
- [WASM 插件阶段管线与沙箱](./plugin-pipeline/index.md) —— 8 个 nginx 对齐阶段、Decision 语义、超时/内存收容、fail_open/fail_closed
- [热重载](./hot-reload/index.md) —— SIGHUP / `POST /openrusty/reload`，原子发布、失败整体拒绝、在途请求与状态保留
- [负载均衡与健康检查](./load-balancing/index.md) —— swrr/ip_hash、被动健康、故障摘除与恢复
- [vllm-kv-scheduler 亲和调度](./vllm-kv-scheduler/index.md) —— vLLM 风格 KV-cache 亲和：task key 粘滞、最少任务选择、TTL 释放
- [运维与部署](./ops/index.md) —— `/openrusty/status`、systemd 开机自启、配置加载、三层验证
- [K8s 三形态](./k8s-forms/index.md) —— sidecar 透明拦截、ingress watch+TLS、egress 三态；iptables-init/inject/chart 部署面

## Changelog
- [changelog](./changelog/)
