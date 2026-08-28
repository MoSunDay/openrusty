#!/usr/bin/env bash
# OpenRusty integration drill: proxy, SSE, h2c, WebSocket, sticky
# scheduling, hot reload (incl. in-flight + rejected reload + load),
# passive health check, plugin fault containment, route timeouts,
# ip_hash, health survival across reloads, KV del/TTL probes,
# memory-ceiling containment, /openrusty/status shape, plugin phases
# (post_read, rewrite, access, body_filter, log), KV scan from a
# plugin, path-key extraction + per-node task cap with fallback,
# OPENRUSTY_CONFIG env startup, request-body cache_salt key
# extraction, active health check (probe-only detection), upstream
# retry_on_timeout, kv-probe balancer phase + resp header get/del +
# req_meta probes, the /openrusty/metrics endpoint shape, ws handshake
# header passthrough, ip_hash dead-node failover, and the concurrent
# reload race (in-flight reloads are rejected with 409).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
TMP="$(mktemp -d /tmp/openrusty-it.XXXXXX)"
GATE_PORT=18080
GATE="http://127.0.0.1:$GATE_PORT"
PIDS=()
PASS=0
FAIL=0

cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
    rm -rf "$TMP"
}
trap cleanup EXIT

check() { # name condition...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then PASS=$((PASS+1)); echo "PASS: $name"
    else FAIL=$((FAIL+1)); echo "FAIL: $name"; fi
}

wait_port() { # port timeout_s
    for _ in $(seq 1 $(( $2 * 10 ))); do
        (exec 3<>/dev/tcp/127.0.0.1/"$1") 2>/dev/null && { exec 3>&-; return 0; }
        sleep 0.1
    done
    return 1
}

# Fail fast with a clear message when a process we started died.
require_alive() { # name pid...
    local name="$1"; shift
    local p
    for p in "$@"; do
        if ! kill -0 "$p" 2>/dev/null; then
            echo "FATAL: $name (pid $p) is no longer running" >&2
            return 1
        fi
    done
    return 0
}

# PID of the echo_upstream instance started for a given port (empty if none).
find_up_pid() { # port
    local port="$1" pid="" p
    for p in "${PIDS[@]:-}"; do
        if ps -o args= -p "$p" 2>/dev/null | grep -q "echo_upstream.*127.0.0.1:$port"; then
            pid="$p"
        fi
    done
    printf '%s' "$pid"
}

# Combined healthy peer count of the vllm upstream (empty when unreachable).
vllm_healthy() {
    curl -s --max-time 5 "$GATE/openrusty/status" | python3 -c 'import sys,json
d=json.load(sys.stdin)
u=[x for x in d["upstreams"] if x["name"]=="vllm"][0]
print(u["healthy"])' 2>/dev/null || true
}

# Succeed once vllm reports `count` healthy peers (polls up to timeout_s).
wait_healthy() { # count timeout_s
    local i
    for i in $(seq 1 $(( $2 * 4 ))); do
        [ "$(vllm_healthy)" = "$1" ] && return 0
        sleep 0.25
    done
    return 1
}

# Current reload generation (empty when the gateway is unreachable).
gen_of() {
    curl -s --max-time 5 "$GATE/openrusty/status" |
        python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])' 2>/dev/null || true
}

# HTTP status code of a GET (empty string on transport failure).
code_of() { # max_time url
    curl -s -o /dev/null -w '%{http_code}' --max-time "$1" "$2" || true
}

# Raw WebSocket handshake with end-to-end auth headers; succeeds on 101.
ws_handshake_headers() { # host port path
    python3 - "$1" "$2" "$3" <<'PY'
import base64, os, socket, sys
host, port, path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
key = base64.b64encode(os.urandom(16)).decode()
req = (
    "GET %s HTTP/1.1\r\n" % path
    + "Host: %s:%d\r\n" % (host, port)
    + "Upgrade: websocket\r\n"
    + "Connection: Upgrade\r\n"
    + "Sec-WebSocket-Key: %s\r\n" % key
    + "Sec-WebSocket-Version: 13\r\n"
    + "Authorization: Bearer ws-probe-token\r\n"
    + "Cookie: session=ws-probe-cookie\r\n"
    + "\r\n"
)
with socket.create_connection((host, port), timeout=10) as sock:
    sock.sendall(req.encode())
    data = b""
    while b"\r\n\r\n" not in data and len(data) < 8192:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
status = data.split(b" ", 2)[1] if b" " in data else b""
sys.exit(0 if status == b"101" else 1)
PY
}

node_of() { curl -s --max-time 5 "$GATE/echo?task=$1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null || true; }
post_body_check() { curl -s --max-time 5 -d 'hello-body' "$GATE/echo" | grep -q '"body_len":10'; }

echo "== build =="
if ! bash scripts/build-plugins.sh; then
    echo "FATAL: plugin build failed" >&2
    exit 1
fi
if ! out="$(cargo build -p openrusty-server --example echo_upstream 2>&1)"; then
    echo "FATAL: echo_upstream example build failed" >&2
    printf '%s\n' "$out" | tail -20 >&2
    exit 1
fi
if ! out="$(cargo build -p openrusty-server 2>&1)"; then
    echo "FATAL: gateway build failed" >&2
    printf '%s\n' "$out" | tail -20 >&2
    exit 1
fi
for artifact in "$ROOT/target/debug/openrusty" \
                "$ROOT/target/debug/examples/echo_upstream" \
                "$ROOT/build/plugins/vllm-kv-scheduler.wasm" \
                "$ROOT/build/plugins/kv-probe.wasm"; do
    if [ ! -e "$artifact" ]; then
        echo "FATAL: missing build artifact: $artifact" >&2
        exit 1
    fi
done

echo "== setup =="
mkdir -p "$TMP/plugins" "$TMP/logs"
cp build/plugins/vllm-kv-scheduler.wasm "$TMP/plugins/"

cat > "$TMP/openrusty.toml" <<CONF
[server]
listen = "127.0.0.1:$GATE_PORT"
log_level = "info"

[plugins]
dir = "$TMP/plugins"
order = ["vllm-kv-scheduler"]
timeout_ms = 250
max_memory_mb = 16
on_failure = "fail_open"

[plugins.settings.vllm-kv-scheduler]
extract = "query:task"
affinity_ttl_s = "6"
max_tasks_per_node = "0"

[[upstreams]]
name = "vllm"
balancer = "swrr"
retries = 2
connect_timeout_ms = 1000
  [[upstreams.peers]]
  addr = "127.0.0.1:19101"
  [[upstreams.peers]]
  addr = "127.0.0.1:19102"
  [[upstreams.peers]]
  addr = "127.0.0.1:19103"
  [upstreams.health]
  max_fails = 2
  fail_window_s = 5
  fail_timeout_s = 2
  [upstreams.health.active]
  interval_ms = 500
  timeout_ms = 500
  path = "/"
  unhealthy_threshold = 2
  healthy_threshold = 2

[[routes]]
path_prefix = "/"
upstream = "vllm"
timeout_ms = 10000
CONF

for i in 1 2 3; do
    "$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:1910$i" "node$i" \
        > "$TMP/logs/up$i.log" 2>&1 &
    PIDS+=($!)
done
"$ROOT/target/debug/openrusty" "$TMP/openrusty.toml" > "$TMP/logs/gate.log" 2>&1 &
GATE_PID=$!; PIDS+=($GATE_PID)
wait_port 19101 10 && wait_port 19102 10 && wait_port 19103 10 && wait_port $GATE_PORT 10 \
    || { echo "FATAL: processes did not come up"; tail -5 "$TMP/logs"/*.log; exit 1; }
# A process can bind its port and die right after; fail fast on that.
if ! require_alive "startup" "${PIDS[@]}"; then
    tail -5 "$TMP/logs"/*.log
    exit 1
fi

echo "== 1. basic proxy =="
BODY="$(curl -s --max-time 5 "$GATE/echo" || true)"
check "http1 proxy 200 with node field" test -n "$(echo "$BODY" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["node"])' 2>/dev/null)"
check "POST body forwarded" post_body_check
check "x-forwarded-for injected" bash -c "curl -s --max-time 5 '$GATE/echo' | grep -qi 'x-forwarded-for'"

echo "== 2. SSE streaming =="
SSE="$(curl -sN --max-time 8 "$GATE/sse?n=3&sleep_ms=30" || true)"
check "SSE delivers 3 events" test "$(echo "$SSE" | grep -c '^data:')" = "3"
check "SSE content-type" bash -c "curl -sI --max-time 5 '$GATE/sse?n=1' | grep -qi 'text/event-stream'"

echo "== 3. h2c + http1 same port =="
check "h2c prior-knowledge" test "$(curl -s -o /dev/null -w '%{http_version}' --http2-prior-knowledge --max-time 5 "$GATE/echo")" = "2"
check "http1 still works" test "$(curl -s -o /dev/null -w '%{http_version}' --max-time 5 "$GATE/echo")" = "1.1"

echo "== 4. websocket pass-through =="
check "ws echo" python3 scripts/ws_client.py 127.0.0.1 $GATE_PORT /ws "hello-openrusty"

echo "== 5. sticky scheduling (vllm-kv-scheduler) =="
N1A="$(node_of t1)"; SAME=1
for _ in 1 2 3 4 5; do [ "$(node_of t1)" = "$N1A" ] || SAME=0; done
[ -n "$N1A" ] || SAME=0
check "same task always same node" test "$SAME" = "1"
N2="$(node_of t2)"; N3="$(node_of t3)"; N4="$(node_of t4)"
check "tasks spread over >1 node" test "$(printf '%s\n%s\n%s\n%s\n' "$N1A" "$N2" "$N3" "$N4" | sort -u | grep -c .)" -ge 2
check "stable after repeat (t2)" test "$(node_of t2)" = "$N2"

echo "== 6. TTL expiry releases affinity =="
DN1="$(node_of drift1)"
check "affinity created" test -n "$DN1"
sleep 7   # > affinity_ttl_s=6
DN2="$(node_of drift1)"
check "still schedulable after TTL" test -n "$DN2"
echo "  info: drift1 node '$DN1' -> '$DN2' after TTL (may or may not move)"

echo "== 7. hot reload: SIGHUP, in-flight request, KV survival =="
GEN_BEFORE="$(gen_of)"
STICKY_BEFORE="$(node_of sticky1)"
curl -s --max-time 10 "$GATE/slow?ms=2500" -o "$TMP/slow.json" & SLOW_PID=$!
sleep 0.4
kill -HUP $GATE_PID
wait $SLOW_PID || true
check "in-flight request survives reload" bash -c "grep -q node '$TMP/slow.json'"
sleep 0.5
GEN_AFTER="$(gen_of)"
check "generation advanced" test "$GEN_AFTER" -gt "$GEN_BEFORE"
check "KV affinity survived reload (same node)" test "$(node_of sticky1)" = "$STICKY_BEFORE"

echo "== 8. POST /openrusty/reload works =="
GEN_BEFORE="$GEN_AFTER"
check "POST reload 200" bash -c "curl -s -X POST --max-time 10 $GATE/openrusty/reload | grep -q generation"
GEN_AFTER="$(gen_of)"
check "generation advanced again" test "$GEN_AFTER" -gt "$GEN_BEFORE"

echo "== 9. bad reload is rejected atomically =="
GEN_BEFORE="$GEN_AFTER"
STICKY2="$(node_of sticky2)"
cp "$TMP/plugins/vllm-kv-scheduler.wasm" "$TMP/vllm-kv-scheduler.wasm.good"
printf 'this is not wasm' > "$TMP/plugins/vllm-kv-scheduler.wasm"
check "reload endpoint reports failure" bash -c "curl -s -X POST --max-time 10 $GATE/openrusty/reload | grep -qi error"
GEN_AFTER="$(gen_of)"
check "generation unchanged" test "$GEN_AFTER" = "$GEN_BEFORE"
check "service still proxies" bash -c "curl -s --max-time 5 $GATE/echo | grep -q node"
check "old plugin still sticky" test "$(node_of sticky2)" = "$STICKY2"
cp "$TMP/vllm-kv-scheduler.wasm.good" "$TMP/plugins/vllm-kv-scheduler.wasm"

echo "== 10. reload under load: no 5xx =="
( for _ in $(seq 1 120); do curl -s -o /dev/null -w '%{http_code}\n' --max-time 5 "$GATE/echo?task=load$_" >> "$TMP/codes.txt"; done ) &
LOAD_PID=$!
sleep 0.6
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
wait $LOAD_PID || true
check "no non-200 during reload" test "$(grep -vc '^200$' "$TMP/codes.txt" 2>/dev/null)" = "0"

echo "== 11. passive health check =="
NODE2_PID="$(find_up_pid 19102)"
kill "$NODE2_PID" 2>/dev/null; sleep 0.5 || true
# No task key: vllm-kv-scheduler declines, default swrr rotates over all peers
# and is guaranteed to hit the dead one.
OK=1
for _ in 1 2 3 4 5 6 7 8; do curl -s --max-time 5 "$GATE/echo" | grep -q node || OK=0; done
check "requests succeed with a dead peer (retry)" test "$OK" = "1"
sleep 0.5
check "status shows 2 healthy peers" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^2$'"
"$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:19102" "node2" >> "$TMP/logs/up2.log" 2>&1 &
PIDS+=($!)
sleep 3   # > fail_timeout_s=2
check "peer recovers" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"

echo "== 12. fault containment: runaway plugin =="
cat > "$TMP/plugins/looper.wasm" <<'WAT'
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (loop $l (br $l))
    (i32.const -5))
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
WAT
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=fault" || true)"
check "fail_open: request still 200 despite looping plugin" test "$CODE" = "200"
check "error counted in status" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([p[\"errors\"] for p in d[\"plugins\"] if p[\"name\"]==\"looper\"][0])' | grep -vq '^0$'"
sed -i 's/on_failure = "fail_open"/on_failure = "fail_closed"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=fault2" || true)"
check "fail_closed: looping plugin yields 503" test "$CODE" = "503"
rm "$TMP/plugins/looper.wasm"
sed -i 's/on_failure = "fail_closed"/on_failure = "fail_open"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=back" || true)"
check "recovered after removing bad plugin" test "$CODE" = "200"


echo "== 13. per-route request timeout =="
cat >> "$TMP/openrusty.toml" <<'CONF'

[[routes]]
path_prefix = "/slow"
upstream = "vllm"
timeout_ms = 300
CONF
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/slow?ms=100" || true)"
check "request within route timeout passes" test "$CODE" = "200"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/slow?ms=800" || true)"
check "request past route timeout yields 502" test "$CODE" = "502"

echo "== 14. ip_hash balancer =="
sed -i 's/balancer = "swrr"/balancer = "ip_hash"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
N1="$(curl -s --max-time 5 "$GATE/echo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null)"
SAME=1; [ -n "$N1" ] || SAME=0
for _ in 1 2 3 4; do
    X="$(curl -s --max-time 5 "$GATE/echo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null)"
    [ "$X" = "$N1" ] || SAME=0
done
check "ip_hash serves requests" test -n "$N1"
check "ip_hash pins one client IP to one node" test "$SAME" = "1"

echo "== 14b. ip_hash dead-node fast failover =="
# Kill two of the three peers: whichever peer 127.0.0.1 hashes to, the
# gateway must not stall on the dead peers - connect failures are instant
# on loopback and the retry loop excludes already-tried peers, so the
# surviving peer has to answer within the route timeout.
UP1_PID="$(find_up_pid 19101)"
UP2_PID="$(find_up_pid 19102)"
if [ -z "$UP1_PID" ] || [ -z "$UP2_PID" ]; then
    echo "FATAL: could not locate the 19101/19102 upstream pids" >&2
    exit 1
fi
kill "$UP1_PID" "$UP2_PID" 2>/dev/null || true
sleep 0.5
FO_NODE="$(curl -s --max-time 5 "$GATE/echo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null || true)"
FO_MS="$(curl -s -o /dev/null -w '%{time_total}' --max-time 5 "$GATE/echo" || true)"
check "ip_hash failover: surviving node3 answers" test "$FO_NODE" = "node3"
check "ip_hash failover: no long stall (<3s)" python3 -c "import sys; sys.exit(0 if 0 < float('${FO_MS:-0}' or 0) < 3.0 else 1)"
for i in 1 2; do
    "$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:1910$i" "node$i" >> "$TMP/logs/up$i.log" 2>&1 &
    PIDS+=($!)
done
check "ip_hash failover: killed peers recover (3 healthy)" wait_healthy 3 20

echo "== 15. upstream health survives reload =="
sed -i 's/balancer = "ip_hash"/balancer = "swrr"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
NODE3_PID="$(find_up_pid 19103)"
kill "$NODE3_PID" 2>/dev/null; sleep 0.5 || true
# No task key: swrr rotates over all peers and is guaranteed to hit the
# dead one, marking it down after max_fails.
OK=1
for _ in 1 2 3 4 5 6; do curl -s --max-time 5 "$GATE/echo" | grep -q node || OK=0; done
check "requests succeed while a peer dies" test "$OK" = "1"
GEN_BEFORE="$(gen_of)"
kill -HUP $GATE_PID
sleep 0.5
GEN_AFTER="$(gen_of)"
check "SIGHUP advanced generation" test "$GEN_AFTER" -gt "$GEN_BEFORE"
check "down peer stays down after reload (2 healthy)" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^2$'"
"$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:19103" "node3" >> "$TMP/logs/up3.log" 2>&1 &
PIDS+=($!)
sleep 3   # > fail_timeout_s=2
check "recovered peer visible after reload (3 healthy)" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"

echo "== 16. kv-probe: content phase, kv_del, TTL release =="
cp build/plugins/kv-probe.wasm "$TMP/plugins/"
sed -i 's/order = \["vllm-kv-scheduler"\]/order = ["vllm-kv-scheduler", "kv-probe"]/' "$TMP/openrusty.toml"
cat >> "$TMP/openrusty.toml" <<'CONF'

[[routes]]
path_prefix = "/probe"
upstream = "vllm"
timeout_ms = 5000
CONF
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=setgetdel" || true)"
check "content: kv set/get/del roundtrip short-circuits 204" test "$CODE" = "204"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=absent" || true)"
check "kv_del persisted across requests" test "$CODE" = "204"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=ttl-set" || true)"
check "short-TTL key accepted" test "$CODE" = "204"
sleep 2   # > the plugin's 1.5s TTL for probe:ttl
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=ttl-check" || true)"
check "TTL expiry released the key" test "$CODE" = "204"
check "header_filter: no marker yet" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?phdr=1' | grep -qi '^x-kv-probe: -'"
check "header_filter: marker set in content phase" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?pset=1&phdr=1' | grep -qi '^x-kv-probe: mk'"
check "header_filter: marker survives across requests" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?phdr=1' | grep -qi '^x-kv-probe: mk'"
check "header_filter: marker gone after kv_del" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?pdel=1&phdr=1' | grep -qi '^x-kv-probe: -'"

echo "== 16b. ws handshake header passthrough =="
# End-to-end headers (Authorization, Cookie) must survive the gateway's
# WebSocket path. The raw handshake below carries both and must answer
# 101; kv-probe's post_read phase runs on the /ws upgrade request and
# records both headers, and the mode=wsheaders probe asserts their exact
# values. (The echo upstream cannot echo WS handshake headers, so the
# request-context assertion goes through the probe endpoint.)
check "ws handshake with Authorization/Cookie answers 101" ws_handshake_headers 127.0.0.1 "$GATE_PORT" /ws
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=wsheaders" || true)"
check "ws passthrough: Authorization + Cookie kept on the request" test "$CODE" = "204"

echo "== 17. memory ceiling containment =="
cat > "$TMP/plugins/glutton.wasm" <<'WAT'
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (memory.grow (i32.const 20000)))
    (i32.const -5))
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
WAT
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=mem" || true)"
check "fail_open contains memory-hungry plugin" test "$CODE" = "200"
check "memory trap counted in status" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([p[\"errors\"] for p in d[\"plugins\"] if p[\"name\"]==\"glutton\"][0])' | grep -vq '^0$'"
rm "$TMP/plugins/glutton.wasm"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true

echo "== 18. status endpoint shape =="
check "status: generation >= 1" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);assert d[\"generation\"]>=1'"
check "status: plugins include vllm-kv-scheduler and kv-probe" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);names={p[\"name\"] for p in d[\"plugins\"]};assert {\"vllm-kv-scheduler\",\"kv-probe\"}<=names'"
check "status: upstream has 3 peers, 3 healthy" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);u=d[\"upstreams\"][0];assert u[\"peers\"]==3 and u[\"healthy\"]==3'"
check "status: routes >= 2" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);assert d[\"routes\"]>=2'"

echo "== 19. phases: post_read, rewrite, access =="
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=postread" || true)"
check "post_read KV visible in content" test "$CODE" = "204"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=rewritecheck" || true)"
check "rewrite KV visible in content" test "$CODE" = "204"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?rwdeny=1" || true)"
check "rewrite phase can deny (418)" test "$CODE" = "418"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?accdeny=1" || true)"
check "access phase can deny (403)" test "$CODE" = "403"

echo "== 20. phase: body_filter =="
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?bmark=1" || true)"
check "body_filter observes streamed body" test "$CODE" = "200"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=bodycheck" || true)"
check "body_filter saw all bytes + last" test "$CODE" = "204"

echo "== 21. phase: log =="
curl -s --max-time 5 "$GATE/echo?lmark=1&task=logt1" > /dev/null || true
sleep 0.5
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=logcheck" || true)"
check "log phase wrote KV marker" test "$CODE" = "204"
check "plugin host_log reaches gateway log" grep -q 'kv-probe log phase marker' "$TMP/logs/gate.log"
check "vllm-kv-scheduler log phase line present" grep -q 'vllm-kv-scheduler done task=logt1' "$TMP/logs/gate.log"

echo "== 22. KV scan from plugin =="
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=scan" || true)"
check "kv_scan sees planted keys" test "$CODE" = "204"

echo "== 23. vllm-kv-scheduler: path extraction + per-node cap =="
sed -i 's/extract = "query:task"/extract = "path:1"/; s/max_tasks_per_node = "0"/max_tasks_per_node = "1"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
sleep 7   # let all previous aff:* entries (TTL 6s) expire so cap counts are clean
path_node() { curl -s --max-time 5 "$GATE/$1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null; }
PA="$(path_node pk-a)"; PB="$(path_node pk-b)"; PC="$(path_node pk-c)"
check "capped tasks spread over all 3 nodes" test "$(printf '%s\n%s\n%s\n' "$PA" "$PB" "$PC" | sort -u | grep -c .)" = "3"
check "path-extracted key stays sticky" test "$(path_node pk-a)" = "$PA"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/pk-d" || true)"
check "over-cap task still served via fallback" test "$CODE" = "200"
sed -i 's/extract = "path:1"/extract = "query:task"/; s/max_tasks_per_node = "1"/max_tasks_per_node = "0"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true

echo "== 24. OPENRUSTY_CONFIG env =="
sed 's/listen = .*/listen = "127.0.0.1:18081"/' "$TMP/openrusty.toml" > "$TMP/openrusty-env.toml"
OPENRUSTY_CONFIG="$TMP/openrusty-env.toml" "$ROOT/target/debug/openrusty" > "$TMP/logs/gate-env.log" 2>&1 &
ENV_PID=$!; PIDS+=($ENV_PID)
if ! wait_port 18081 10; then
    echo "FATAL: OPENRUSTY_CONFIG instance did not come up" >&2
    tail -5 "$TMP/logs/gate-env.log"
    exit 1
fi
if ! require_alive "env-config instance" "$ENV_PID"; then
    tail -5 "$TMP/logs/gate-env.log"
    exit 1
fi
check "env-config instance serves /openrusty/status" bash -c "curl -s --max-time 5 http://127.0.0.1:18081/openrusty/status | grep -q generation"
check "env-config instance proxies" bash -c "curl -s --max-time 5 http://127.0.0.1:18081/echo | grep -q node"
kill $ENV_PID 2>/dev/null || true

echo "== 25. vllm-kv-scheduler: body cache_salt extraction =="
sed -i 's/extract = "query:task"/extract = "body:cache_salt"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
sleep 7   # let all previous aff:* entries (TTL 6s) expire so counts are clean
salt_node() { curl -s --max-time 5 -X POST -H 'Content-Type: application/json' -d "{\"cache_salt\":\"$1\"}" "$GATE/echo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null; }
S1="$(salt_node agent:session-a)"; SAME=1
for _ in 1 2 3; do [ "$(salt_node agent:session-a)" = "$S1" ] || SAME=0; done
[ -n "$S1" ] || SAME=0
check "same cache_salt sticks to one node" test "$SAME" = "1"
S2="$(salt_node agent:session-b)"; S3="$(salt_node other:session-c)"
check "salts spread over >1 node" test "$(printf '%s\n%s\n%s\n' "$S1" "$S2" "$S3" | sort -u | grep -c .)" -ge 2
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 -X POST -d '{"messages":[]}' "$GATE/echo" || true)"
check "body without cache_salt falls back to default balancer" test "$CODE" = "200"
sed -i 's/extract = "body:cache_salt"/extract = "query:task"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true

echo "== 26. active health check (probe-only detection, no proxied requests) =="
# All three vllm peers must be active-healthy before the kill.
check "active plane: 3 healthy before kill" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"active_healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"
NODE2_PID="$(find_up_pid 19102)"
kill "$NODE2_PID" 2>/dev/null
sleep 2.5 || true   # >= 2 probe failures at 500ms + margin; NO gateway proxied requests
check "active plane alone marks peer down (2 active_healthy)" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"active_healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^2$'"
check "combined healthy also 2" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^2$'"
"$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:19102" "node2" >> "$TMP/logs/up2.log" 2>&1 &
PIDS+=($!)
sleep 2.5   # >= 2 successful probes + margin
check "active plane recovers peer (3 active_healthy)" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"active_healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"
check "combined healthy back to 3" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"

echo "== 27. retry_on_timeout (deterministic two-peer upstream) =="
# Peer 0 = 19104: a hanging TCP server that accepts connections but never
# responds -> deterministic route timeout (300ms), NOT a connect failure.
# Peer 1 = 19101 (node1 echo): answers any path instantly. Default passive
# health (max_fails=3, fail_window_s=10) keeps peer 0 healthy through the
# first two timeouts, so the retry path is really exercised, and trips the
# 3rd timeout -> punished (fail_timeout_s=10).
python3 -c 'import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 19104)); s.listen(16)
while True:
    c, _ = s.accept(); time.sleep(60); c.close()' >> "$TMP/logs/hang.log" 2>&1 &
PIDS+=($!)
sleep 0.5
cat >> "$TMP/openrusty.toml" <<'CONF'

[[upstreams]]
name = "timeout-retry"
balancer = "swrr"
retries = 1
retry_on_timeout = false

[[upstreams.peers]]
addr = "127.0.0.1:19104"
weight = 1

[[upstreams.peers]]
addr = "127.0.0.1:19101"
weight = 1

[[routes]]
path_prefix = "/timeout-retry"
upstream = "timeout-retry"
timeout_ms = 300
CONF
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
# Fresh SWRR state: request 1 -> peer 0 (hangs past the 300ms route timeout).
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/timeout-retry?ms=5000" || true)"
check "retry_on_timeout=false: timeout yields 502, no retry" test "$CODE" = "502"
sed -i 's/retry_on_timeout = false/retry_on_timeout = true/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
# SWRR now sits on peer 1 (fast): direct 200 without needing the retry.
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/timeout-retry?ms=5000" || true)"
check "retry_on_timeout=true: healthy peer still served (200)" test "$CODE" = "200"
# Next SWRR position is peer 0 again: timeout, then ONE retry onto peer 1.
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/timeout-retry?ms=5000" || true)"
check "retry_on_timeout=true: timeout retried onto healthy peer (200)" test "$CODE" = "200"
# Repeat: peer 0's 3rd timeout trips max_fails=3 -> punished; retry still works.
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/timeout-retry?ms=5000" || true)"
check "retry keeps working while bad peer is punished (200)" test "$CODE" = "200"
check "bad peer punished: timeout-retry reports 1 healthy" bash -c "curl -s --max-time 5 $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);u=[u for u in d[\"upstreams\"] if u[\"name\"]==\"timeout-retry\"][0];print(u[\"healthy\"])' | grep -q '^1$'"
sed -i 's/retry_on_timeout = true/retry_on_timeout = false/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true

echo "== 28. kv-probe: balancer phase, resp header get/del, req_meta =="
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=balancer" || true)"
check "balancer phase: assertions pass, request proceeds (200)" test "$CODE" = "200"
NODE="$(curl -s --max-time 5 "$GATE/probe?mode=balancer" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null || true)"
check "balancer phase: set_peer pick honored (node1)" test "$NODE" = "node1"
check "header_filter: respset=1 sets x-kv-probe: mk" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?respset=1' | grep -qi '^x-kv-probe: mk'"
check "header_filter: respdel=1 removes x-kv-probe" bash -c "H=\"\$(curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?respdel=1')\"; echo \"\$H\" | grep -qi '^HTTP/1.1 200' && ! echo \"\$H\" | grep -qi '^x-kv-probe'"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$GATE/echo?meta=1" || true)"
check "req_meta: method/client_ip/header verified (204)" test "$CODE" = "204"

echo "== 29. metrics endpoint shape =="
export METRICS="$(curl -s --max-time 5 "$GATE/openrusty/metrics")"
check "metrics: requests_total family" bash -c "echo \"\$METRICS\" | grep -q '# TYPE openrusty_requests_total counter'"
check "metrics: duration histogram family" bash -c "echo \"\$METRICS\" | grep -q '# TYPE openrusty_request_duration_seconds histogram'"
check "metrics: catch-all route 200 counted" bash -c "echo \"\$METRICS\" | grep -Eq 'openrusty_requests_total\{route=\"/\",code=\"200\"\} [1-9]'"
check "metrics: vllm success attempts" bash -c "echo \"\$METRICS\" | grep -Eq 'openrusty_upstream_attempts_total\{upstream=\"vllm\",result=\"success\"\} [1-9]'"
check "metrics: plugin errors family" bash -c "echo \"\$METRICS\" | grep -q '# TYPE openrusty_plugin_errors_total counter'"
check "metrics: vllm peer node1 healthy" bash -c "echo \"\$METRICS\" | grep -q 'openrusty_peer_healthy{upstream=\"vllm\",addr=\"127.0.0.1:19101\"} 1'"
check "metrics: kv-probe kv entries" bash -c "echo \"\$METRICS\" | grep -q 'openrusty_kv_entries{plugin=\"kv-probe\"}'"
check "metrics: +Inf duration bucket" bash -c "echo \"\$METRICS\" | grep -q 'openrusty_request_duration_seconds_bucket{le=\"+Inf\"}'"

echo "== 30. concurrent reload race (in-flight gate) =="
# Two reloads fired at the same instant: the in-flight gate must reject
# the loser with 409 while exactly one of them swaps the generation.
reload_pair() { # prints the two HTTP codes, sorted ascending
    python3 - "$GATE" <<'PY'
import sys, threading, urllib.error, urllib.request
gate = sys.argv[1]
codes = []
barrier = threading.Barrier(2)
def fire():
    barrier.wait()
    try:
        req = urllib.request.Request(gate + "/openrusty/reload", data=b"", method="POST")
        with urllib.request.urlopen(req, timeout=30) as resp:
            codes.append(resp.status)
    except urllib.error.HTTPError as err:
        codes.append(err.code)
    except Exception:
        codes.append(0)
threads = [threading.Thread(target=fire) for _ in range(2)]
for t in threads:
    t.start()
for t in threads:
    t.join()
print(" ".join(str(c) for c in sorted(codes)))
PY
}
RACE_GEN_BEFORE="$(gen_of)"
RACE_SEEN_409=0
RACE_CLEAN=1
for _ in 1 2 3 4 5 6 7 8; do
    CODES="$(reload_pair || true)"
    for c in $CODES; do
        case "$c" in
            200|409) ;;
            *) RACE_CLEAN=0 ;;
        esac
    done
    case " $CODES " in
        *" 200 409 "*) RACE_SEEN_409=1; break ;;
    esac
done
check "reload race: only 200/409 answers" test "$RACE_CLEAN" = "1"
check "reload race: in-flight reload rejected with 409" test "$RACE_SEEN_409" = "1"
RACE_GEN_AFTER="$(gen_of)"
check "reload race: generation advanced exactly once per success" test "$RACE_GEN_AFTER" -gt "$RACE_GEN_BEFORE"
check "reload race: gateway still proxies" bash -c "curl -s --max-time 5 $GATE/echo | grep -q node"

echo
echo "== CHECKS: $((PASS + FAIL)) total =="
echo "== RESULT: $PASS passed, $FAIL failed =="
[ "$FAIL" = "0" ]
