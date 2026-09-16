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
| `-4` | NGX_DONE – stop the phase chain; short-circuit with an empty 204, upgraded to `200 + resp_body_set` body when the plugin wrote one (from `content`: no proxy attempt) |
| `100..=599` | deny: abort the request with this HTTP status |
| anything else | protocol error – treated per `on_failure` policy |

Denying with a status outside `100..=599` has no wire representation: such a
`Deny` must be returned as `-1` (NGX_ERROR). SDKs encode an out-of-range
`Deny` as `-1` automatically. The host treats `-1` (like any code outside
the table) as a bad code and applies the plugin `on_failure` policy; it is
never decoded as `Deny(0)` or an HTTP status.

## Phases

| id | name | runs |
|----|------|------|
| 0 | post_read | after routing (route index + upstream already on the context) |
| 1 | rewrite | after post_read |
| 2 | access | after rewrite |
| 3 | content | after request body buffering (WebSocket requests bypass it), before the default proxy handler |
| 4 | balancer | before each upstream attempt (may pick the peer) |
| 5 | header_filter | when upstream response headers arrived |
| 6 | body_filter | once per response body data chunk (observe-only) plus one final empty chunk (`last=true`) |
| 7 | log | after the body stream ends (success/error/disconnect) or once for short-circuited requests |

Routing happens before any plugin phase: the request path is matched against
the configured `[[routes]]` by longest prefix (first match wins on ties), and
the matched route index and upstream name are set on the request context
before `post_read` runs. When no route matches, the request is answered with
404 and no plugin phase runs at all — `log` included.

`body_filter` is observe-only: each response body data chunk is pushed through
the phase (readable via `body_chunk` / `body_is_last`) and then yielded to the
client unchanged. After the stream ends — normally, on error, or on client
disconnect — the phase is called one final time with an empty chunk and
`body_is_last() == 1`. Returning `Done` or `Declined` from `body_filter` does
not change the stream: nginx-style body rewriting is not implemented.

## Host imports (namespace `openrusty`)

Two-phase reads: the host writes into a guest-provided buffer.

- Return `>= 0`: bytes written.
- Return `< 0`: buffer too small; `-ret` is the required length (grow, retry).
  Exception: `req_peer_get` returns `-2` for an out-of-range index (nothing is
  written; not a required length).
- Guest-side cap: one read can never exceed the SDK's 1 MiB scratch arena
  (`HEAP_SIZE`). A host requirement above the cap fails cleanly (the SDK
  wrapper returns `None`) instead of growing the buffer into an arena
  exhaustion trap.

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
req_peer_count() -> i32                     # total peers of the routed upstream
req_peer_get(idx: i32, out_ptr: i32, out_cap: i32) -> i32
  writes TLV: name, addr, healthy("1"/"0"); -2 = invalid index (nothing written)
balancer_set_peer(idx: i32) -> i32          # 0 ok, -1 out of bounds (no health check)

kv_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_cap: i32) -> i32
  0 = not found
kv_set(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32, ttl_ms: i64) -> i32
  0 = stored; -1 = refused (value over 64 KiB, or the plugin's live KV total
  of key+value bytes would exceed 1 MiB, or an unreadable pointer); a refused
  set leaves the store unchanged
kv_del(key_ptr: i32, key_len: i32) -> i32
kv_scan_begin(prefix_ptr: i32, prefix_len: i32) -> i32
  cursor id >= 0; -1 = no cursor (unreadable prefix, or the per-plugin
  cursor cap of 64 live cursors is full)
kv_scan_next(cursor: i32, out_ptr: i32, out_cap: i32) -> i32
  writes one TLV pair (key, value); 0 = exhausted;
  -1 = cursor invalid/expired (dead: stop iterating; do not end it again);
  any other negative value = -(required capacity): grow the buffer and
  retry the same element; the cursor stays valid (the host consumes the
  entry only once the guest actually received it)
kv_scan_end(cursor: i32)

kv_scan cursors are snapshot-based; a cursor idle for more than 60 s is
reclaimed automatically, so a guest that abandons a scan cannot leak it.

resp_header_get(name_ptr: i32, name_len: i32, out_ptr: i32, out_cap: i32) -> i32
resp_header_set(name_ptr: i32, name_len: i32, val_ptr: i32, val_len: i32) -> i32
resp_header_del(name_ptr: i32, name_len: i32) -> i32
resp_body_set(ptr: i32, len: i32) -> i32
  # writes the response body for a short-circuited response; >= 0 = bytes
  # accepted (== len), -1 = refused (over the 1 MiB cap or unreadable
  # pointer). Effects: Decision::Done + body -> 200 + body (instead of the
  # empty 204); Decision::Deny(s) + body -> s + body. Replaces any body
  # written earlier in the same request. Usable in any phase. This is
  # also the response channel of the dynamic execution API (see below).

Chain and late-write semantics: the body is carried per request across the
plugin chain, and a plugin that writes no body preserves one written by an
earlier plugin (an explicit write replaces it; last writer wins). A body
written after the terminal decision was taken (`header_filter`/`log` in
the dynamic API, `log` in the pipeline short-circuits) still upgrades the
would-be empty 204 to `200 + body` on both paths, because the response is
assembled after the log phase; the pipeline's access-log status for a
`Done` short-circuit is sampled before the log phase and therefore logs
204 in that edge.

body_chunk(out_ptr: i32, out_cap: i32) -> i32   # body_filter: current chunk
  returns bytes written; 0 = no data (final empty chunk call); negative = error
body_is_last() -> i32                           # 1 if chunk is the final one

cfg_get(key_ptr: i32, key_len: i32, out_ptr: i32, out_cap: i32) -> i32
  # plugin settings from [plugins.settings.<name>]; 0 if unset
```

`req_peer_count` / `req_peer_get` expose every peer of the routed upstream
(healthy or not) with the health flag captured at request start.
`balancer_set_peer` only bounds-checks the index; it never checks health. The
gateway re-validates the plugin's pick against the current health registry
before each attempt and falls back to the configured default balancer when
the chosen peer is invalid or unhealthy (see `pipeline_peer.rs`).

## Timeouts & memory

- Each phase call runs under wasmtime epoch interruption, driven by an
  engine-scoped epoch ticker: one background thread per engine bumps the
  engine epoch every 10 ms, and every call sets its own relative epoch
  deadline at start (`plugins.timeout_ms`), so a call is never cut short by
  a stale watchdog from an earlier one. Traps never crash the host.
- Module instantiation is bounded the same way: a 5-second epoch deadline,
  so a non-terminating start section fails loading with an
  instantiation-timeout error instead of hanging the gateway.
- `StoreLimits` caps instance memory at `plugins.max_memory_mb`; growing past
  it traps and is handled by the failure policy.
- `on_failure = fail_open` (default): trap/timeout -> decision Declined, the
  request proceeds. `fail_closed`: trap/timeout -> Deny(503). Both bump the
  plugin's error counter (visible in `/openrusty/status`).

## Hot reload

- `SIGHUP` or `POST /openrusty/reload` (loopback only; an optional `[admin]
token` additionally requires `Authorization: Bearer`/`X-OpenRusty-Token`) re-reads the config,
  validates it, compiles every plugin module in the background, and publishes
  a new snapshot atomically. Any failure aborts the whole reload and keeps
  the previous snapshot. In-flight requests keep their snapshot alive via
  `Arc`. Per-plugin shared state (KV store) survives reloads keyed by plugin
  name, and upstream health survives reloads keyed by upstream name (peer
  state is kept when the peer list shape is unchanged).
- Concurrent reloads publish via compare-and-swap: every success advances
  the snapshot generation by exactly one. A reload that loses the publish
  race rebuilds against the fresh snapshot (3 attempts) and then fails with
  a conflict error, leaving the previous snapshot untouched.

## Dynamic execution API

`[dynamic]` config section (absent = disabled) serves
`POST /api/v1/dynamic/<name>` by running `<dynamic.dir>/<name>.wasm` as a
synthesized single-plugin pipeline: post_read -> rewrite -> access ->
content -> header_filter -> log (no balancer, no body_filter, no proxy).
Module names must match `^[A-Za-z0-9][A-Za-z0-9._-]*$` (else 400); a
missing file answers 404.

- Decision mapping: `Done` + `resp_body_set` body -> 200 + body (module
  headers verbatim except content-length; default
  `content-type: text/plain; charset=utf-8` when a body exists and the
  module set none); `Done` without a body -> empty 204; `Deny(s)` -> s
  with the body when one was set; no terminal response produced -> 500.
  A body written as late as `header_filter`/`log` still upgrades a
  body-less `Done` from 204 to 200 (the outcome is assembled after
  `log`).
- The module cache is stat-driven: entries are keyed by the file's
  `(mtime, size)`, compiled singleflight per name, so replacing a module
  takes effect on the next request with no reload. Per-name host state
  (`HostState`: KV + error counters) survives module replacement.
- Config keys: `dir` (overridable via the `OPENRUSTY_DYNAMIC_DIR` env),
  `timeout_ms`, `max_memory_mb`, `on_failure`, `max_body_bytes` (POST
  cap, 413 above), and `[dynamic.settings.<name>]` free-form settings
  readable via `cfg_get`. Route mounting is boot-time; reload rebuilds
  the registry when the section changes but cannot mount/unmount routes.
- Registration face (admin plane, behind the `[admin]` token guard when
  one is set):
  - `PUT /openrusty/dynamic/{name}` - body is the module artifact (wasm
    binary or WAT text, 64 MiB cap -> 413). The bytes are validated
    (compile + ABI) BEFORE anything lands on disk, then stored via
    temp-file + atomic rename; the stat-driven cache serves the new
    module on the next request, no reload. Optional query
    `method=M&path=P` binds the module at the same time; an EMPTY body
    with both params binds an already-present artifact (404 if the file
    is missing, 400 if only one param is sent).
  - `DELETE /openrusty/dynamic/{name}` - removes the artifact and every
    binding pointing at it (404 when neither existed).
  - `GET /openrusty/dynamic` - lists modules on disk and live bindings.
- Route bindings: `{METHOD, base path} -> module`, from `[[dynamic.routes]]`
  config entries and runtime `PUT ...?method=&path=` calls. The gateway
  fallback consults the binding table BEFORE proxy route matching, so
  bindings win over `[[routes]]` prefixes (the fixed
  `POST /api/v1/dynamic/<name>` route keeps precedence over bindings).
  A binding hit ALSO takes precedence over WebSocket upgrade handling
  on the bound path: the module sees and answers the handshake request
  instead of the upgrade being transparently proxied.
  Semantics: methods are matched case-insensitively; `/api/*` and `/api`
  share one slot (a later bind of either shape replaces the earlier);
  exact paths are checked before the longest prefix; a `/api/*` prefix
  matches `/api` itself and everything below it, never `/apifoo`.
  Dispatched requests run the same synthesized pipeline as the fixed
  route and count only in `openrusty_dynamic_requests_total{module,code}`
  (no per-route histogram). Reload semantics: config-declared bindings
  are re-enforced (stale ones dropped) on every reload while runtime
  bindings survive; removing the `[dynamic]` section empties the table
  (dispatch never hijacks the pipeline).
- Example modules: `plugins/dynamic-echo` (settings + module headers) and
  `plugins/dynamic-reverse` (used to demo replace-without-reload).

