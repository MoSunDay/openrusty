# OpenRusty WASM Plugin ABI (v1)

Plugins are `cdylib` crates compiled to `wasm32-unknown-unknown` (no WASI).
One `.wasm` file = one plugin. The host instantiates each module once per
request with a fresh `Store` (memory limits enforced per store); compiled
`Module`s are cached and hot-swapped atomically on reload.

## Module shape

- Linear memory: the guest exports its default memory (no explicit export
  name required; `memory` index 0).
- No WASI imports are linked. Only the `openrusty` import namespace below.
- Guest allocator: the guest exports `orr_alloc(size: i32) -> i32` (returns a
  pointer) and may export `orr_dealloc(ptr: i32, size: i32)`. The host uses
  `orr_alloc` for every host->guest data hand-off. Missing `orr_alloc` fails
  ABI validation at load time; `orr_dealloc` is optional.

## Guest export

```text
orr_on_phase(phase: i32, ctx: i32) -> i32
```

- `phase`: one of the phase ids below.
- `ctx`: reserved; always `0` in v1 (single request per store).
- Return value (nginx-aligned):

| code | meaning |
|------|---------|
| `0` | NGX_OK – handled, continue chain |
| `-5` | NGX_DECLINED – pass, continue chain |
| `-4` | NGX_DONE – stop the phase chain (`content`: short-circuit empty 204) |
| `100..=599` | deny: abort the request with this HTTP status |
| anything else | protocol error – treated per `on_failure` policy |

## Phases

| id | name | runs |
|----|------|------|
| 0 | post_read | before routing |
| 1 | rewrite | before routing |
| 2 | access | before routing |
| 3 | content | before the default proxy handler |
| 4 | balancer | before each upstream attempt (may pick the peer) |
| 5 | header_filter | when upstream response headers arrived |
| 6 | body_filter | once per response body chunk |
| 7 | log | after the response is fully sent or on error |

## Host imports (namespace `openrusty`)

Two-phase reads: the host writes into a guest-provided buffer.

- Return `>= 0`: bytes written.
- Return `< 0`: buffer too small; `-ret` is the required length (grow, retry).

`kv_get` returns `0` for a missing key (empty values are indistinguishable
from absent; plugins must not store empty values). Strings are raw UTF-8
bytes, lists are TLV-encoded: `u32 len | bytes` repeated (u32 little-endian).

```text
host_log(level: i32, ptr: i32, len: i32)
host_now_ms() -> i64

req_meta(key_ptr: i32, key_len: i32, out_ptr: i32, out_cap: i32) -> i32
  keys: "method" "path" "query" "version" "client_ip" "upstream"
        "header:<name>" (first value) "headers" (TLV list of "k: v")
        "body" (raw request body bytes, capped at 16 MiB; buffered before
        the content phase, so content/balancer/header_filter/body_filter/log
        see it while post_read/rewrite/access see an empty body)
req_peer_count() -> i32                     # healthy peers of routed upstream
req_peer_get(idx: i32, out_ptr: i32, out_cap: i32) -> i32
  writes TLV: name, addr, healthy("1"/"0")
balancer_set_peer(idx: i32) -> i32          # 0 ok, -1 invalid/unhealthy

kv_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_cap: i32) -> i32
  0 = not found
kv_set(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32, ttl_ms: i64) -> i32
kv_del(key_ptr: i32, key_len: i32) -> i32
kv_scan_begin(prefix_ptr: i32, prefix_len: i32) -> i32   # cursor id >= 0
kv_scan_next(cursor: i32, out_ptr: i32, out_cap: i32) -> i32
  writes one TLV pair (key, value); 0 = exhausted; negative = invalid cursor
kv_scan_end(cursor: i32)

resp_header_get(name_ptr: i32, name_len: i32, out_ptr: i32, out_cap: i32) -> i32
resp_header_set(name_ptr: i32, name_len: i32, val_ptr: i32, val_len: i32) -> i32
resp_header_del(name_ptr: i32, name_len: i32) -> i32

body_chunk(out_ptr: i32, out_cap: i32) -> i32   # body_filter: current chunk
  returns bytes written; 0 = no data (should not happen); negative = error
body_is_last() -> i32                           # 1 if chunk is the final one

cfg_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_cap: i32) -> i32
  # plugin settings from [plugins.settings.<name>]; 0 if unset
```

## Timeouts & memory

- Each phase call runs under wasmtime epoch interruption; a watchdog bumps the
  engine epoch after `plugins.timeout_ms`. Traps never crash the host.
- `StoreLimits` caps instance memory at `plugins.max_memory_mb`; growing past
  it traps and is handled by the failure policy.
- `on_failure = fail_open` (default): trap/timeout -> decision Declined, the
  request proceeds. `fail_closed`: trap/timeout -> Deny(503). Both bump the
  plugin's error counter (visible in `/openrusty/status`).

## Hot reload

- `SIGHUP` or `POST /openrusty/reload` (loopback only) re-reads the config,
  validates it, compiles every plugin module in the background, and publishes
  a new snapshot atomically. Any failure aborts the whole reload and keeps
  the previous snapshot. In-flight requests keep their snapshot alive via
  `Arc`. Per-plugin shared state (KV store) and upstream health survive
  reloads (keyed by name).
