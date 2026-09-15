# integration drill section 30: kv-probe content/kv_del/TTL, ws handshake
# header passthrough, memory ceiling, status shape, phases (post_read,
# rewrite, access, body_filter, log), KV scan, path-key cap (sections 16-23).

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
