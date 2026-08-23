#!/usr/bin/env bash
# OpenRusty integration drill: proxy, SSE, h2c, WebSocket, sticky
# scheduling, hot reload (incl. in-flight + rejected reload + load),
# passive health check, plugin fault containment, route timeouts,
# ip_hash, health survival across reloads, KV del/TTL probes, and
# memory-ceiling containment.
set -u
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
TMP="$(mktemp -d /tmp/openrusty-it.XXXXXX)"
GATE_PORT=18080
GATE="http://127.0.0.1:$GATE_PORT"
PIDS=()
PASS=0
FAIL=0

cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
    sleep 0.3
    for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null; done
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

node_of() { curl -s --max-time 5 "$GATE/echo?task=$1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null; }
post_body_check() { curl -s --max-time 5 -d 'hello-body' "$GATE/echo" | grep -q '"body_len":10'; }

echo "== build =="
bash scripts/build-plugins.sh || exit 1
cargo build -p openrusty-server --example echo_upstream 2>&1 | tail -1
cargo build -p openrusty-server 2>&1 | tail -1

echo "== setup =="
mkdir -p "$TMP/plugins" "$TMP/logs"
cp build/plugins/kv-scheduler.wasm "$TMP/plugins/"

cat > "$TMP/openrusty.toml" <<CONF
[server]
listen = "127.0.0.1:$GATE_PORT"
log_level = "info"

[plugins]
dir = "$TMP/plugins"
order = ["kv-scheduler"]
timeout_ms = 250
max_memory_mb = 16
on_failure = "fail_open"

[plugins.settings.kv-scheduler]
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

echo "== 1. basic proxy =="
BODY="$(curl -s --max-time 5 "$GATE/echo")"
check "http1 proxy 200 with node field" test -n "$(echo "$BODY" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d["node"])' 2>/dev/null)"
check "POST body forwarded" post_body_check
check "x-forwarded-for injected" bash -c "curl -s --max-time 5 '$GATE/echo' | grep -qi 'x-forwarded-for'"

echo "== 2. SSE streaming =="
SSE="$(curl -sN --max-time 8 "$GATE/sse?n=3&sleep_ms=30")"
check "SSE delivers 3 events" test "$(echo "$SSE" | grep -c '^data:')" = "3"
check "SSE content-type" bash -c "curl -sI --max-time 5 '$GATE/sse?n=1' | grep -qi 'text/event-stream'"

echo "== 3. h2c + http1 same port =="
check "h2c prior-knowledge" test "$(curl -s -o /dev/null -w '%{http_version}' --http2-prior-knowledge --max-time 5 "$GATE/echo")" = "2"
check "http1 still works" test "$(curl -s -o /dev/null -w '%{http_version}' --max-time 5 "$GATE/echo")" = "1.1"

echo "== 4. websocket pass-through =="
check "ws echo" python3 scripts/ws_client.py 127.0.0.1 $GATE_PORT /ws "hello-openrusty"

echo "== 5. sticky scheduling (kv-scheduler) =="
N1A="$(node_of t1)"; SAME=1
for _ in 1 2 3 4 5; do [ "$(node_of t1)" = "$N1A" ] || SAME=0; done
[ -n "$N1A" ] || SAME=0
check "same task always same node" test "$SAME" = "1"
N2="$(node_of t2)"; N3="$(node_of t3)"; N4="$(node_of t4)"
check "tasks spread over >1 node" test "$(printf '%s\n%s\n%s\n%s\n' "$N1A" "$N2" "$N3" "$N4" | sort -u | wc -l)" -ge 2
check "stable after repeat (t2)" test "$(node_of t2)" = "$N2"

echo "== 6. TTL expiry releases affinity =="
DN1="$(node_of drift1)"
check "affinity created" test -n "$DN1"
sleep 7   # > affinity_ttl_s=6
DN2="$(node_of drift1)"
check "still schedulable after TTL" test -n "$DN2"
echo "  info: drift1 node '$DN1' -> '$DN2' after TTL (may or may not move)"

echo "== 7. hot reload: SIGHUP, in-flight request, KV survival =="
GEN_BEFORE="$(curl -s $GATE/openrusty/status | python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])')"
STICKY_BEFORE="$(node_of sticky1)"
curl -s --max-time 10 "$GATE/slow?ms=2500" -o "$TMP/slow.json" & SLOW_PID=$!
sleep 0.4
kill -HUP $GATE_PID
wait $SLOW_PID
check "in-flight request survives reload" bash -c "grep -q node '$TMP/slow.json'"
sleep 0.5
GEN_AFTER="$(curl -s $GATE/openrusty/status | python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])')"
check "generation advanced" test "$GEN_AFTER" -gt "$GEN_BEFORE"
check "KV affinity survived reload (same node)" test "$(node_of sticky1)" = "$STICKY_BEFORE"

echo "== 8. POST /openrusty/reload works =="
GEN_BEFORE="$GEN_AFTER"
check "POST reload 200" bash -c "curl -s -X POST --max-time 10 $GATE/openrusty/reload | grep -q generation"
GEN_AFTER="$(curl -s $GATE/openrusty/status | python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])')"
check "generation advanced again" test "$GEN_AFTER" -gt "$GEN_BEFORE"

echo "== 9. bad reload is rejected atomically =="
GEN_BEFORE="$GEN_AFTER"
STICKY2="$(node_of sticky2)"
cp "$TMP/plugins/kv-scheduler.wasm" "$TMP/kv-scheduler.wasm.good"
printf 'this is not wasm' > "$TMP/plugins/kv-scheduler.wasm"
check "reload endpoint reports failure" bash -c "curl -s -X POST --max-time 10 $GATE/openrusty/reload | grep -qi error"
GEN_AFTER="$(curl -s $GATE/openrusty/status | python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])')"
check "generation unchanged" test "$GEN_AFTER" = "$GEN_BEFORE"
check "service still proxies" bash -c "curl -s --max-time 5 $GATE/echo | grep -q node"
check "old plugin still sticky" test "$(node_of sticky2)" = "$STICKY2"
cp "$TMP/kv-scheduler.wasm.good" "$TMP/plugins/kv-scheduler.wasm"

echo "== 10. reload under load: no 5xx =="
( for _ in $(seq 1 120); do curl -s -o /dev/null -w '%{http_code}\n' --max-time 5 "$GATE/echo?task=load$_" >> "$TMP/codes.txt"; done ) &
LOAD_PID=$!
sleep 0.6
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
wait $LOAD_PID
check "no non-200 during reload" test "$(grep -vc '^200$' "$TMP/codes.txt" 2>/dev/null)" = "0"

echo "== 11. passive health check =="
NODE2_PID=""
for i in "${!PIDS[@]}"; do
    if [ "$(ps -o args= -p "${PIDS[$i]}" 2>/dev/null | grep -c 19102)" = "1" ]; then NODE2_PID="${PIDS[$i]}"; fi
done
kill "$NODE2_PID" 2>/dev/null; sleep 0.5
# No task key: kv-scheduler declines, default swrr rotates over all peers
# and is guaranteed to hit the dead one.
OK=1
for _ in 1 2 3 4 5 6 7 8; do curl -s --max-time 5 "$GATE/echo" | grep -q node || OK=0; done
check "requests succeed with a dead peer (retry)" test "$OK" = "1"
sleep 0.5
check "status shows 2 healthy peers" bash -c "curl -s $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^2$'"
"$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:19102" "node2" >> "$TMP/logs/up2.log" 2>&1 &
PIDS+=($!)
sleep 3   # > fail_timeout_s=2
check "peer recovers" bash -c "curl -s $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"

echo "== 12. fault containment: runaway plugin =="
cat > "$TMP/plugins/looper.wasm" <<'WAT'
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (loop $l (br $l))
    (i32.const -5))
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
WAT
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=fault")"
check "fail_open: request still 200 despite looping plugin" test "$CODE" = "200"
check "error counted in status" bash -c "curl -s $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([p[\"errors\"] for p in d[\"plugins\"] if p[\"name\"]==\"looper\"][0])' | grep -vq '^0$'"
sed -i 's/on_failure = "fail_open"/on_failure = "fail_closed"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=fault2")"
check "fail_closed: looping plugin yields 503" test "$CODE" = "503"
rm "$TMP/plugins/looper.wasm"
sed -i 's/on_failure = "fail_closed"/on_failure = "fail_open"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=back")"
check "recovered after removing bad plugin" test "$CODE" = "200"


echo "== 13. per-route request timeout =="
cat >> "$TMP/openrusty.toml" <<'CONF'

[[routes]]
path_prefix = "/slow"
upstream = "vllm"
timeout_ms = 300
CONF
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/slow?ms=100")"
check "request within route timeout passes" test "$CODE" = "200"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/slow?ms=800")"
check "request past route timeout yields 502" test "$CODE" = "502"

echo "== 14. ip_hash balancer =="
sed -i 's/balancer = "swrr"/balancer = "ip_hash"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
N1="$(curl -s --max-time 5 "$GATE/echo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null)"
SAME=1; [ -n "$N1" ] || SAME=0
for _ in 1 2 3 4; do
    X="$(curl -s --max-time 5 "$GATE/echo" | python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null)"
    [ "$X" = "$N1" ] || SAME=0
done
check "ip_hash serves requests" test -n "$N1"
check "ip_hash pins one client IP to one node" test "$SAME" = "1"

echo "== 15. upstream health survives reload =="
sed -i 's/balancer = "ip_hash"/balancer = "swrr"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
NODE3_PID=""
for i in "${!PIDS[@]}"; do
    if [ "$(ps -o args= -p "${PIDS[$i]}" 2>/dev/null | grep -c 19103)" = "1" ]; then NODE3_PID="${PIDS[$i]}"; fi
done
kill "$NODE3_PID" 2>/dev/null; sleep 0.5
# No task key: swrr rotates over all peers and is guaranteed to hit the
# dead one, marking it down after max_fails.
OK=1
for _ in 1 2 3 4 5 6; do curl -s --max-time 5 "$GATE/echo" | grep -q node || OK=0; done
check "requests succeed while a peer dies" test "$OK" = "1"
GEN_BEFORE="$(curl -s $GATE/openrusty/status | python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])')"
kill -HUP $GATE_PID
sleep 0.5
GEN_AFTER="$(curl -s $GATE/openrusty/status | python3 -c 'import sys,json;print(json.load(sys.stdin)["generation"])')"
check "SIGHUP advanced generation" test "$GEN_AFTER" -gt "$GEN_BEFORE"
check "down peer stays down after reload (2 healthy)" bash -c "curl -s $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^2$'"
"$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:19103" "node3" >> "$TMP/logs/up3.log" 2>&1 &
PIDS+=($!)
sleep 3   # > fail_timeout_s=2
check "recovered peer visible after reload (3 healthy)" bash -c "curl -s $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([u[\"healthy\"] for u in d[\"upstreams\"]][0])' | grep -q '^3$'"

echo "== 16. kv-probe: content phase, kv_del, TTL release =="
cp build/plugins/kv-probe.wasm "$TMP/plugins/"
sed -i 's/order = \["kv-scheduler"\]/order = ["kv-scheduler", "kv-probe"]/' "$TMP/openrusty.toml"
cat >> "$TMP/openrusty.toml" <<'CONF'

[[routes]]
path_prefix = "/probe"
upstream = "vllm"
timeout_ms = 5000
CONF
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=setgetdel")"
check "content: kv set/get/del roundtrip short-circuits 204" test "$CODE" = "204"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=absent")"
check "kv_del persisted across requests" test "$CODE" = "204"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=ttl-set")"
check "short-TTL key accepted" test "$CODE" = "204"
sleep 2   # > the plugin's 1.5s TTL for probe:ttl
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/probe?mode=ttl-check")"
check "TTL expiry released the key" test "$CODE" = "204"
check "header_filter: no marker yet" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?phdr=1' | grep -qi '^x-kv-probe: -'"
check "header_filter: marker set in content phase" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?pset=1&phdr=1' | grep -qi '^x-kv-probe: mk'"
check "header_filter: marker survives across requests" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?phdr=1' | grep -qi '^x-kv-probe: mk'"
check "header_filter: marker gone after kv_del" bash -c "curl -s -D - -o /dev/null --max-time 5 '$GATE/echo?pdel=1&phdr=1' | grep -qi '^x-kv-probe: -'"

echo "== 17. memory ceiling containment =="
cat > "$TMP/plugins/glutton.wasm" <<'WAT'
(module
  (func (export "orr_on_phase") (param i32 i32) (result i32)
    (drop (memory.grow (i32.const 20000)))
    (i32.const -5))
  (func (export "orr_alloc") (param i32) (result i32) i32.const 0)
  (memory (export "memory") 1))
WAT
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/echo?task=mem")"
check "fail_open contains memory-hungry plugin" test "$CODE" = "200"
check "memory trap counted in status" bash -c "curl -s $GATE/openrusty/status | python3 -c 'import sys,json;d=json.load(sys.stdin);print([p[\"errors\"] for p in d[\"plugins\"] if p[\"name\"]==\"glutton\"][0])' | grep -vq '^0$'"
rm "$TMP/plugins/glutton.wasm"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null
echo
echo "== RESULT: $PASS passed, $FAIL failed =="
[ "$FAIL" = "0" ]
