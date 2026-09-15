# integration drill section 40: OPENRUSTY_CONFIG env startup, body
# cache_salt key extraction, active health check (sections 24-26).

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
