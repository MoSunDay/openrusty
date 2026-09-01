#!/usr/bin/env bash
# OpenRusty M3.b local egress drill: the [egress] tri-mode (direct |
# gateway | deny) exercised under a real iptables OUTPUT REDIRECT, all
# inside one throwaway network namespace. Companion to
# scripts/local-netns-test.sh (which covers the inbound plane and the
# iptables-init parameter surface; this drill focuses on egress policy).
#
# Topology (nothing leaks to the host; everything is cleaned on EXIT):
#   netns orr-egress-<pid>
#     10.124.0.2:18080  echo_upstream "app"     (original destination)
#     10.124.0.2:18081  python3 TCP echo        (opaque workload)
#     10.124.0.3:18080  same app via a loopback VIP (drill target)
#     0.0.0.0:15001     SECOND openrusty acting as the egress gateway
#                       (plain HTTP inbound, routes / -> app)
#     0.0.0.0:4140      openrusty outbound listener (transparent); the
#                       process under test - its [egress] mode is rewritten
#                       and the process restarted between phases
#     0.0.0.0:4191      openrusty admin (metrics)
#   iptables nat inside the netns, installed directly (the init parameter
#   surface is covered by the m1 drill):
#     OUTPUT -> ORREG:  -m owner --uid-owner $GATE_UID -j RETURN
#                       -p tcp -j REDIRECT --to-ports 4140
#   Both gateways run as UID 65534, so their own dials (egress proxy ->
#   gateway, gateway -> app) bypass the REDIRECT; the drill's own curl
#   probes run as root and ARE intercepted - exactly the production
#   sidecar shape.
#
# Phases (each counted PASS/FAIL, style of scripts/integration.sh):
#   direct  - 20 intercepted conns tunnel verbatim to the original
#             destination (200s from the app); outcome counter
#             egress_direct == 20 and the outbound sample count matches.
#   deny    - 20 intercepted conns fail closed (every probe refused);
#             egress_deny == 20, sample count matches.
#   gateway - 20 HTTP conns are forwarded through the second openrusty
#             (200 from the app, the gateway access log records
#             path=/echo, and the app sees an x-forwarded-for appended by
#             the gateway hop - 127.0.0.1); one opaque TCP probe is
#             refused; egress_gateway_ok == 20, egress_deny == 1,
#             gateway_fail == 0, sample count matches 20 + 1.
#
# Every disposition is read back from
# /openrusty/metrics -> openrusty_transparent_conns_total{role,outcome}:
# the observability contract IS the assertion surface.
#
# Environment requirements are checked up front; without netns/iptables
# capability the script prints SKIP(local-egress): <reason> and exits 0.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
TMP=""
NS=""
VH=""
PIDS=()
PASS=0
FAIL=0
SKIP_TAG="local-egress"
NET_TAG="egress"
APP_IP=10.124.0.2
VIP_IP=10.124.0.3
APP_PORT=18080
RAW_PORT=18081
EG_PORT=4140
ADM_PORT=4191
GW2_PORT=15001
SAMPLES=20
EG_PID=""

# Shared helpers (checks, gates, ports, teardown) live in lib-netns.sh.
. "$(dirname "$0")/lib-netns.sh"
# Port probes must reach the proxy ports even after the REDIRECT is
# installed: probe as the exempt UID.
PROBE_UID="$GATE_UID"

egress_rules() { # install|remove - the OUTPUT hijack (drill-owned chain)
    case "$1" in
    install)
        ip netns exec "$NS" "$IPT" -t nat -N ORREG 2>/dev/null || true
        ip netns exec "$NS" "$IPT" -t nat -A ORREG \
            -m owner --uid-owner "$GATE_UID" -j RETURN
        ip netns exec "$NS" "$IPT" -t nat -A ORREG -p tcp -j REDIRECT \
            --to-ports "$EG_PORT"
        ip netns exec "$NS" "$IPT" -t nat -A OUTPUT -j ORREG
        ;;
    remove)
        ip netns exec "$NS" "$IPT" -t nat -D OUTPUT -j ORREG >/dev/null 2>&1 || true
        ip netns exec "$NS" "$IPT" -t nat -F ORREG >/dev/null 2>&1 || true
        ip netns exec "$NS" "$IPT" -t nat -X ORREG >/dev/null 2>&1 || true
        ;;
    esac
}

cleanup() {
    [ -n "$NS" ] && egress_rules remove
    netns_teardown
}
trap cleanup EXIT

env_gates
build_artifacts
netns_up "$APP_IP" "$VIP_IP"

start_gateway2() {
    # The egress gateway: a second openrusty with a PLAIN inbound listener
    # (no transparency) and normal routing. `request finished` access-log
    # lines (log_level = info) are the path proof for the gateway phase.
    cat > "$TMP/gateway.toml" <<CONF
[server]
listen = "0.0.0.0:$GW2_PORT"
log_level = "info"

[plugins]
dir = "$TMP/plugins"
timeout_ms = 250
max_memory_mb = 16
on_failure = "fail_open"

[[upstreams]]
name = "app"
balancer = "swrr"
connect_timeout_ms = 1000
  [[upstreams.peers]]
  addr = "$APP_IP:$APP_PORT"

[[routes]]
path_prefix = "/"
upstream = "app"
timeout_ms = 10000
CONF
    ip netns exec "$NS" setpriv --reuid "$GATE_UID" --regid "$GATE_UID" \
        --clear-groups "$ROOT/target/debug/openrusty" "$TMP/gateway.toml" \
        > "$TMP/logs/gateway.log" 2>&1 &
    PIDS+=($!)
}

start_egress() { # mode - (re)write [egress] and start the process under test
    local mode="$1"
    cat > "$TMP/egress.toml" <<CONF
[server]
listen = "127.0.0.1:4599"
log_level = "debug"

[[server.listeners]]
role = "outbound"
listen = "0.0.0.0:$EG_PORT"
transparent = true

[[server.listeners]]
role = "admin"
listen = "0.0.0.0:$ADM_PORT"

[plugins]
dir = "$TMP/plugins"
timeout_ms = 250
max_memory_mb = 16
on_failure = "fail_open"

[egress]
mode = "$mode"
CONF
    if [ "$mode" = "gateway" ]; then
        printf 'gateway = "127.0.0.1:%s"\n' "$GW2_PORT" >> "$TMP/egress.toml"
    fi
    ip netns exec "$NS" setpriv --reuid "$GATE_UID" --regid "$GATE_UID" \
        --clear-groups "$ROOT/target/debug/openrusty" "$TMP/egress.toml" \
        > "$TMP/logs/eg-$mode.log" 2>&1 &
    EG_PID=$!
    PIDS+=("$EG_PID")
    # Wait on the admin port ONLY: a probe at the outbound port would
    # connect to the proxy directly (exempt UID, no orig dst) and pollute
    # the disposition counters with a no_orig_dst sample. The drill's
    # first intercepted conns prove the outbound listener is serving.
    wait_port "$ADM_PORT" 10 || {
        echo "FATAL: egress proxy ($mode) did not come up"
        tail -n 5 "$TMP/logs/eg-$mode.log" >&2
        exit 1
    }
}

stop_egress() { # label - SIGTERM and require the clean three-phase exit
    kill -TERM "$EG_PID" 2>/dev/null || true
    EG_RC=0
    wait "$EG_PID" || EG_RC=$?
    check "$1: SIGTERM exit code 0" test "$EG_RC" = "0"
}

metric() { # outcome -> sum of that egress outcome's counters
    ip netns exec "$NS" setpriv --reuid "$GATE_UID" --regid "$GATE_UID" \
        --clear-groups curl -s --max-time 5 \
        "http://127.0.0.1:$ADM_PORT/openrusty/metrics" |
        awk -v want="outcome=\"$1\"" '
            $1 ~ /^openrusty_transparent_conns_total\{/ && index($1, want) { sum += $NF }
            END { print sum + 0 }'
}

metric_outbound_total() { # -> sum of every outbound sample (all outcomes)
    ip netns exec "$NS" setpriv --reuid "$GATE_UID" --regid "$GATE_UID" \
        --clear-groups curl -s --max-time 5 \
        "http://127.0.0.1:$ADM_PORT/openrusty/metrics" |
        awk '
            $1 ~ /^openrusty_transparent_conns_total\{/ &&
            index($1, "role=\"outbound\"") { sum += $NF }
            END { print sum + 0 }'
}

drive_http_ok() { # n url outfile -> prints how many probes answered 200
    local n="$1" url="$2" out="$3" ok=0 code
    for _ in $(seq 1 "$n"); do
        code="$(ns_curl -o "$out" -w '%{http_code}' --max-time 5 "$url" || true)"
        [ "$code" = "200" ] && ok=$((ok + 1))
    done
    echo "$ok"
}

drive_conn_refused() { # n url -> prints how many probes failed to answer
    local n="$1" url="$2" refused=0 rc
    for _ in $(seq 1 "$n"); do
        rc=0
        ns_curl -o /dev/null --max-time 5 "$url" >/dev/null 2>&1 || rc=$?
        [ "$rc" != "0" ] && refused=$((refused + 1))
    done
    echo "$refused"
}

opaque_probe_got_bytes_back() { # dst_ip dst_port -> prints GOTBACK|CLEAN
    ip netns exec "$NS" python3 - "$1" "$2" <<'PY' 2>/dev/null
import socket, sys
host, port = sys.argv[1], int(sys.argv[2])
try:
    s = socket.create_connection((host, port), timeout=5)
    s.sendall(b"\x16\x03\x01 orr-egress opaque probe\r\n\r\n")
    s.settimeout(3)
    data = s.recv(4096)
    sys.stdout.write("GOTBACK" if data else "CLEAN")
except Exception:
    sys.stdout.write("CLEAN")
PY
}

echo "== topology services =="
ip netns exec "$NS" "$ROOT/target/debug/examples/echo_upstream" \
    "0.0.0.0:$APP_PORT" app > "$TMP/logs/app.log" 2>&1 &
PIDS+=($!)
ip netns exec "$NS" python3 -c '
import asyncio
async def handle(r, w):
    try:
        while True:
            d = await r.read(65536)
            if not d:
                break
            w.write(d)
            await w.drain()
    except Exception:
        pass
    finally:
        try:
            w.close()
            await w.wait_closed()
        except Exception:
            pass
async def main():
    s = await asyncio.start_server(handle, "0.0.0.0", '"$RAW_PORT"')
    async with s:
        await s.serve_forever()
asyncio.run(main())
' > "$TMP/logs/raw.log" 2>&1 &
PIDS+=($!)
start_gateway2
start_egress direct
require_alive "startup" "${PIDS[@]}" || { tail -n 5 "$TMP/logs"/*.log >&2; exit 1; }
egress_rules install
echo "OUTPUT REDIRECT installed (owner UID $GATE_UID exempt, --to-ports $EG_PORT)"

echo "== phase 1: mode = direct (tunnel to orig_dst) =="
OK="$(drive_http_ok "$SAMPLES" "http://$VIP_IP:$APP_PORT/echo" "$TMP/direct.body")"
check "d1: all $SAMPLES intercepted conns answered 200" test "$OK" = "$SAMPLES"
check "d1: response body comes from the app" grep -q '"node":"app"' "$TMP/direct.body"
check "d1: egress_direct counter == $SAMPLES" test "$(metric egress_direct)" = "$SAMPLES"
check "d1: no egress_deny samples" test "$(metric egress_deny)" = "0"
check "d1: outbound sample count == conns driven" \
    test "$(metric_outbound_total)" = "$SAMPLES"
stop_egress "d2"

echo "== phase 2: mode = deny (fail closed) =="
start_egress deny
REFUSED="$(drive_conn_refused "$SAMPLES" "http://$VIP_IP:$APP_PORT/echo")"
check "e1: all $SAMPLES intercepted conns refused" test "$REFUSED" = "$SAMPLES"
check "e1: egress_deny counter == $SAMPLES" test "$(metric egress_deny)" = "$SAMPLES"
check "e1: egress_direct counter still == $SAMPLES (fresh process)" \
    test "$(metric egress_direct)" = "0"
check "e1: outbound sample count == conns driven" \
    test "$(metric_outbound_total)" = "$SAMPLES"
stop_egress "e2"

echo "== phase 3: mode = gateway (forward to the second openrusty) =="
start_egress gateway
OK="$(drive_http_ok "$SAMPLES" "http://$VIP_IP:$APP_PORT/echo" "$TMP/gateway.body")"
check "g1: all $SAMPLES intercepted conns answered 200" test "$OK" = "$SAMPLES"
check "g1: response body comes from the app" grep -q '"node":"app"' "$TMP/gateway.body"
check "g1: gateway access log saw the forwarded requests" \
    bash -c "test \$(plain_log '$TMP/logs/gateway.log' | grep -c 'request finished') -ge $SAMPLES"
check "g1: app sees the gateway hop in x-forwarded-for (127.0.0.1)" \
    grep -q '127.0.0.1' "$TMP/gateway.body"
# NOTE: check() expands its condition as "$@", so a leading "!" would be
# executed as a command name instead of negating; compare a verdict instead.
check "g1: opaque TCP refused (payload did not return)" \
    test "$(opaque_probe_got_bytes_back "$APP_IP" "$RAW_PORT")" = "CLEAN"
check "g1: egress_gateway_ok counter == $SAMPLES" \
    test "$(metric egress_gateway_ok)" = "$SAMPLES"
check "g1: egress_gateway_fail counter == 0" test "$(metric egress_gateway_fail)" = "0"
check "g1: opaque refusal counted as egress_deny == 1" test "$(metric egress_deny)" = "1"
check "g1: outbound sample count == conns driven ($SAMPLES + 1 opaque)" \
    test "$(metric_outbound_total)" = "$((SAMPLES + 1))"
stop_egress "g2"

echo
echo "== CHECKS: $((PASS + FAIL)) total =="
echo "== RESULT: $PASS passed, $FAIL failed =="
[ "$FAIL" = "0" ]
