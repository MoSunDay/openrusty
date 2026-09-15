# integration drill section 20: hot reload (SIGHUP/POST, in-flight + bad
# reload, reload under load), passive health, plugin fault containment,
# route timeout, ip_hash (+ failover), health across reloads (sections 7-15).

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
