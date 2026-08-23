# OpenRusty

An nginx-like API gateway framework in Rust. Plugins are standalone WebAssembly
modules loaded at runtime and hot-reloaded without restarting the server —
the gateway binary and the plugins are fully decoupled.

Built on `axum` + `hyper-util` (same-port HTTP/1.1 and h2c) and `wasmtime`.

## Features

- **Staged WASM plugins** — nginx request phases: `post_read`, `rewrite`,
  `access`, `content`, `balancer`, `header_filter`, `body_filter`, `log`.
  Plugins trap/timeout inside a sandbox; `fail_open` (default) or
  `fail_closed` policy decides the fallback. See `docs/wasm-abi.md`.
- **Hot reload** — `SIGHUP` or `POST /openrusty/reload` (loopback only):
  re-reads the config, validates it, compiles every plugin in the background,
  and publishes the new snapshot atomically. Any failure rejects the whole
  reload; in-flight requests finish on the old snapshot; per-plugin KV state
  and upstream health survive reloads.
- **Gateway basics** — upstreams with smooth weighted round-robin or
  `ip_hash`, passive health checks, failure retries on another peer,
  WebSocket pass-through, SSE streaming, request timeouts.
- **`kv-scheduler` plugin** — vLLM-style KV-cache affinity: a task key
  extracted from the URL sticks to one peer; new tasks go to the peer with
  the fewest active tasks (tie: oldest last-schedule time); affinity entries
  expire by TTL, releasing the slot.

## Layout

| path | purpose |
|---|---|
| `crates/openrusty-core` | config types, request context, phase/decision semantics |
| `crates/openrusty-wasm` | wasmtime runtime: ABI imports, sandboxed runner, hot-reload registry |
| `crates/openrusty-proxy` | upstreams, balancers, passive health, pooled clients, forwarding |
| `crates/openrusty-server` | the `openrusty` binary: h2c accept loop, phase pipeline, reload endpoint |
| `crates/openrusty-sdk` | `no_std` guest SDK (imports, allocator, `dispatch!`) |
| `crates/openrusty-macros` | `#[phase(...)]` proc macro |
| `plugins/kv-scheduler` | first-party plugin (own workspace, wasm-only) |
| `plugins/kv-probe` | drill plugin exercising KV host calls and content/header_filter phases |
| `docs/wasm-abi.md` | the plugin ABI contract |
| `config/openrusty.example.toml` | annotated example configuration |

## Quick start

```bash
# 1. build the plugins (independent of the server build)
bash scripts/build-plugins.sh

# 2. build the gateway
cargo build -p openrusty-server

# 3. run (adjust config/openrusty.example.toml first)
./target/debug/openrusty config/openrusty.example.toml

# 4. inspect / hot-reload
curl localhost:8180/openrusty/status
kill -HUP $(pgrep -x openrusty)          # or:
curl -X POST localhost:8180/openrusty/reload
```

### Run as a service (autostart)

```bash
cargo build --release -p openrusty-server
cargo build --release -p openrusty-server --example echo_upstream
sudo bash scripts/install-service.sh
```

Installs and enables `openrusty.service` (gateway on `127.0.0.1:8180`) plus
three demo echo upstreams `openrusty-echo@9001/9002/9003.service`, all
started now and on boot. The script is idempotent: re-running it restarts
the units so freshly built binaries are picked up. Logs live in journald
(`journalctl -u openrusty`); stop/disable with
`systemctl disable --now openrusty.service` (same for the echo instances).

## Tests

```bash
cargo test --workspace        # unit tests (balancers, TTL, ABI, sandbox, reload)
bash scripts/build-plugins.sh # per-plugin unit tests + wasm build
bash scripts/integration.sh   # end-to-end drill: 67 checks, sections 1-24
```

The integration drill starts echo upstreams and the gateway on test ports
(18080/191xx) and walks through `scripts/integration.sh` sections 1-24:
basic proxy, SSE, h2c, WebSocket, sticky scheduling with TTL release, hot
reload (SIGHUP / POST, in-flight requests, KV survival, atomic rejection of
broken plugins, reload under load with zero 5xx), passive health checks,
runaway-plugin containment under both failure policies, per-route request
timeouts, `ip_hash` pinning, upstream health surviving reloads, the
`kv-probe` plugin (content phase, `kv_del`, TTL release, header_filter), and
memory-ceiling containment of a `memory.grow`-hungry plugin. Sections 18-24
round out the remaining `kv-probe` phases (`post_read`, `rewrite`, `access`,
`body_filter`, `log`) plus plugin-driven `kv_scan`, assert the
`/openrusty/status` JSON shape (generation, plugin list, upstream health,
route count), exercise `kv-scheduler` path-segment key extraction with a
`max_tasks_per_node` cap (capped tasks spread across peers; an over-cap task
falls back to the default balancer), and boot a second gateway instance from
the `OPENRUSTY_CONFIG` environment variable.
