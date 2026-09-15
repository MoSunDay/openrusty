# 生命周期补齐：-t 干跑 / 日志轮转 / socket activation 热重启 / 信号语义

## Context
- nginx 对照下网关生命周期有四个缺口：① reload 失败的第一大来源（插件编译错误）在启动前无低成本探测手段；② 日志只能进 stdout/journal，无法配合 logrotate；③ `systemctl restart` 期间端口失守（旧进程退出到新进程 bind 之间的 SYN 被拒）；④ SIGQUIT 落在 tokio 默认行为（core dump），运维语义未定义。
- 决策：热重启选 systemd socket activation（fd 继承）而非 SO_REUSEPORT/USR2——部署已全量 systemd，代码面最小，且同时覆盖计划内重启与 `Restart=on-failure` 崩溃重启；USR2 二进制热升级**延后**（触发条件：grace 窗口外的长连接存活或非 systemd 部署；届时复用 `sd_listen` 继承模块 + exec 自身 + PID 文件）。

## Change Summary
### A. `openrusty -t [CONFIG]` 配置干跑（W1）
- `crates/openrusty-server/src/check.rs`：`run(&Path) -> Result<Summary, String>` = `load_config` + `validate` + `PluginRegistry::bootstrap`（复用启动路径全量编译/ABI 校验，不新写逻辑）；打印 effective listeners、路由/upstream 数、插件清单（phase order）、`syntax is ok`。坏配置/坏插件退出码 1，不启动服务。
### B. 日志文件 + SIGUSR1 reopen（W2）
- `[server] log_file`（可选，缺省 = 现状 stdout/journal）。`crates/openrusty-server/src/logging.rs`：`ReopenableWriter`（`Arc<Mutex<File>>` 包路径，实现 `io::Write` 喂 `tracing_appender::non_blocking`）+ `reopen()`（重开路径、锁内换句柄）；main 里 SIGUSR1 任务调 reopen（**无条件注册**——stdout 模式下忽略并记 debug，避免误杀进程）。logrotate 契约：外部 rename + `systemctl kill -s SIGUSR1`，滚动策略归 logrotate。
### C. systemd socket activation 热重启（W3）
- `crates/openrusty-server/src/sd_listen.rs`：解析 `LISTEN_FDS`/`LISTEN_PID`/`LISTEN_FDNAMES`（env 读取可注入、纯函数可测）→ 按**端口匹配** effective listeners（数量/端口不匹配 = fail-fast，打印 expected vs received）→ `from_raw_fd` → nonblocking → `tokio::net::TcpListener`，零新依赖。模块按可复用设计（未来 USR2 exec 传递走同一入口）。
- `listeners.rs` bind 路径改为 inherit-or-bind：有 fd 则继承（日志 `listener inherited (socket activation)`），否则照旧 bind。
- `scripts/install-service.sh`：从安装配置提取 listeners 生成 `openrusty.socket`（每 listener 一条 `ListenStream=`，`Backlog=511`）；service 加 `Requires=/After=openrusty.socket` + 注释掉的 `# User=openrusty` 降权提示（socket 由 systemd 以 root 绑定后网关可降权——补 nginx `user` 指令缺口）。
- 重启语义（全部复用现有代码）：`systemctl restart` → SIGTERM → 三阶段排空在途 → 期间 systemd 持 socket、新连接进 backlog → 新进程继承 fd 立即消费，**0 拒连**。
### D. SIGQUIT 快速退出（W4）
- `shutdown.rs` 新增 `run_fast`：停接入、跳过 grace 排空、在途计为 forced、exit 0（与 `run(grace=0)` 不同：仍 await accept 任务、task_errors 照常计数）。`main.rs` select 增加 SIGQUIT 分支。
- 信号表：HUP=reload；TERM/INT=优雅排空（grace）；QUIT=快速退出；USR1=日志 reopen；USR2=reserved for binary upgrade（未实现）。

## 测试覆盖
| 面 | 载体 |
|----|------|
| -t 干跑（好/坏 TOML、坏插件、validate 拒绝） | `check.rs` 单测 5 例；演练 §干跑 3 checks |
| reopen 语义（rename 后新写落新文件、append 不截断、坏路径 fail-fast） | `logging.rs` 单测 3 例；演练 §log reopen 5 checks |
| fd 解析/端口匹配（pid 不符、leftover、重复端口、真实 fd 继承 reorder+accept） | `sd_listen.rs` 单测 6 例 |
| 压载重启 0 拒连（fork+exec 传 fd、SIGTERM 排空、同 socket 复活） | 演练 §socket activation 5 checks：completed=200 / connect_errors=0 / inherited ×2 |
| QUIT 快退 vs TERM 排空对比 | `shutdown.rs` 单测 1 例；演练 §SIGQUIT 3 checks |
| install-service.sh（socket 单元生成、listener 提取、幂等） | 沙箱 stub 验证（不触宿主 systemd） |

- `scripts/integration.sh` 拆分为 orchestrator（163 行）+ `scripts/integration/*.sh` 分节（各 ≤151 行）以遵守文件行数上限； drills 119 → **135 checks** 全绿。
- 汇总：`cargo test --workspace` 513 passed / 0 failed；`scripts/build-plugins.sh` 4 wasm。

## Impact Surface
- 默认零变化：不设 `log_file`、无 LISTEN_FDS env 时行为与之前逐字节一致（stdout 日志、自行 bind）。新增均为 opt-in（-t / log_file / socket 单元）。
