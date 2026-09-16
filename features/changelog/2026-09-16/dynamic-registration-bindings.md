# 动态 WASM API P1+P2：HTTP 注册面 + {method,path}→module 路由绑定

## Context
- P0（`POST /api/v1/dynamic/<name>` 单模块执行 API）落地后，模块上架仍需“拷文件 + 手工 reload”，且动态模块无法接管任意路径——与“随时可上新”的闭环目标还差两步。
- P3（存储/DB host imports）明确不在本轮范围。

## Change Summary
### P1 注册面（管理面，走既有 `[admin]` token 守卫）
- `PUT /openrusty/dynamic/{name}`：body 即模块工件（wasm 二进制或 WAT 文本，64 MiB 上限 413）。**先验证后落盘**——`DynamicRegistry::validate_bytes`（编译 + ABI 检查）在 `spawn_blocking` 上跑，`Bytes::clone` 仅引用计数；落盘走 `<name>.wasm.put-<pid>-<seq>` 临时文件 + 原子 rename（临时名不带 `.wasm` 后缀，解析器不可见）。stat 驱动缓存下一条请求即生效，无需 reload。
- 空 body + `method`&`path` 查询参数 = **仅绑定**已存在的工件（文件投递的模块也能获得路由；缺文件 404，只传一个参数 400）；空 body 无参数 400。
- `DELETE /openrusty/dynamic/{name}`：删工件 + 清所有指向它的绑定（二者皆无则 404）；`GET /openrusty/dynamic`：列盘上模块与在线绑定。
- 载体：`crates/openrusty-server/src/dynamic_admin.rs`（229 行）+ `dynamic_admin_tests.rs`（316 行，8 例）。

### P2 路由绑定表
- `DynamicRoutes`（`crates/openrusty-server/src/dynamic_routes.rs`）：`exact` HashMap + 最长前缀优先的 `prefix` Vec + `config_keys` 集合，容量上限 4096；网关 fallback 在代理路由匹配**之前**查表——绑定胜过 `[[routes]]` 前缀（固定 `POST /api/v1/dynamic/<name>` 仍是真实路由，优先级更高）。
- 语义（单测固化）：method 大小写不敏感（归一化大写）；`/api/*` 与 `/api` 共用一个槽位，后绑替换先绑（无论形状）；精确匹配先于最长前缀；前缀 `/api/*` 匹配 `/api` 本身与其下一切，不匹配 `/apifoo`。
- 配置面：`[[dynamic.routes]]`（method/path/module，`crates/openrusty-core/src/config/dynamic.rs` 校验：t字符 method、≤1024 路径、base key 去重）。
- reload 语义：`apply_dynamic_routes` 每次 reload 重申配置键（配置里消失即摘除），运行时绑定存活；`[dynamic]` 节移除则清表；registry 缺席时 `route_hit` 恒 None（绝不劫持管线）。
- 计量：派发请求只计 `openrusty_dynamic_requests_total{module,code}`，不进 per-route 直方图（与固定路由一致）。

## 测试覆盖
| 面 | 载体 |
|----|------|
| 配置校验（坏 method/path/module、base key 重复拒绝） | `config/dynamic.rs` 单测 4 例 |
| 绑定表（槽位替换、前缀边界、表容量、reconcile 配置/运行时键） | `dynamic_routes.rs` 单测 |
| 注册面（存取+绑定一体、仅绑定、删除级联、token 守卫、垃圾字节 400、特性关闭 404） | `dynamic_admin_tests.rs` 8 例 |
| e2e：配置绑定、PUT 一体、仅绑定、槽位替换、method 域、拒绝矩阵、DELETE 级联、reload 存活/重申、计量 | 演练 §31（lib-dynamic-drill.sh +22 checks） |

## Impact Surface
- 默认路径零变化：无 `[[dynamic.routes]]` 且不调用注册面 ⇔ 旧语义。
- 测试环境踩坑（已修）：本机 9001 常驻 echo 服务会让“未命中 → 404”类断言经由 catch-all 代理路由变成 200——单测侧将代理路由收窄到 `/__proxy`，e2e 侧负向断言改为“模块标记不出现”。wasmtime `Module::new` 接受 WAT 文本，测试直接以 WAT 充当 `.wasm` 工件。
- clippy：新增代码零告警（顺手修掉 state.rs/dynamic_admin.rs 两处自引入告警）。

## Related Docs
- [docs/wasm-abi.md](../../docs/wasm-abi.md)（注册面 + 绑定语义 + reload 语义）
- [config/openrusty.example.toml](../../config/openrusty.example.toml)（`[[dynamic.routes]]` 示例）
