# integration drill section 10: proxy basics - http1/POST/XFF, SSE,
# h2c + http1 same port, websocket pass-through, sticky scheduling,
# TTL affinity expiry (drill sections 1-6).

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
