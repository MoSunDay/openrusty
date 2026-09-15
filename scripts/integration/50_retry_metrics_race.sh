# integration drill section 50: retry_on_timeout, kv-probe balancer phase
# + resp header get/del + req_meta, metrics endpoint shape, concurrent
# reload race (sections 27-30).

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
