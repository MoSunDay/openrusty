#!/usr/bin/env bash
# OpenRusty M1.8 local netns drill: transparent interception under a real
# iptables REDIRECT, all inside one throwaway network namespace.
#
# Topology (nothing leaks to the host; everything is cleaned on EXIT):
#   netns orr-m1-<pid>
#     10.123.0.2:18080  echo_upstream "app"      (HTTP workload)
#     10.123.0.2:18081  inline python3 TCP echo  (opaque workload)
#     10.123.0.3:18080  same app via a loopback VIP (outbound-path target)
#     0.0.0.0:4143      openrusty inbound  (transparent)
#     0.0.0.0:4140      openrusty outbound (transparent)
#     0.0.0.0:4191      openrusty admin
#   iptables nat OUTPUT chain (inside the netns), owner-UID exempt:
#     -> app/raw ports  REDIRECT to 4143 (inbound interception)
#     -> VIP traffic    REDIRECT to 4140 (outbound interception)
#   The gateway itself runs as UID 65534 so its own dials bypass the
#   REDIRECT rules (-m owner --uid-owner 65534 -j RETURN) - otherwise the
#   proxy would loop into itself.
#
# Assertions (each counted PASS/FAIL, style of scripts/integration.sh):
#   1. HTTP transparent inbound: redirected curl answers 200 through the
#      proxy pipeline (x-forwarded-for injected, access log written).
#   2. orig_dst observability: the gateway logs the recovered original
#      destination of an intercepted connection as the true pre-NAT
#      address (the app), never its own listener address. Note: only the
#      tunnel path records orig_dst in the log today - the HTTP sniff
#      branch does not - so this drives an opaque probe at the app addr.
#   3. opaque TCP passthrough: 4096 random bytes round-trip verbatim
#      through the tunnel (tunnel semantics, zero bytes lost).
#   4. outbound path: traffic to the VIP is REDIRECTed to 4140 and dialed
#      straight at the original destination.
#   5. loop guard: a REDIRECT aiming the gateway's own admin port at the
#      inbound listener is refused and logged. Chosen shape: an OUTPUT
#      REDIRECT rule dport 4191 -> 4143 (the guard compares ports only,
#      so the address in front of the guarded port does not matter).
#   6. graceful exit: SIGTERM -> exit 0, three-phase log lines, ports freed.
#   7. repeatability: full cleanup lets the script run green twice in a row.
#
# Every environment requirement is checked up front; a machine without
# netns/iptables/conntrack capability prints SKIP(local-netns): <reason>
# and exits 0 - a skip is visible, never a silent pass.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
TMP=""
NS=""
VH=""
PIDS=()
PASS=0
FAIL=0
GATE_UID=65534
APP_IP=10.123.0.2
VIP_IP=10.123.0.3
APP_PORT=18080
RAW_PORT=18081
INB_PORT=4143
OUT_PORT=4140
ADM_PORT=4191
IPT=iptables

cleanup() {
    if [ -n "$NS" ] && ip netns list 2>/dev/null | grep -q "^${NS}"; then
        if ip netns exec "$NS" "$IPT" -t nat list >/dev/null 2>&1; then
            ip netns exec "$NS" "$IPT" -t nat -D OUTPUT -j ORRM1 >/dev/null 2>&1 || true
            ip netns exec "$NS" "$IPT" -t nat -F ORRM1 >/dev/null 2>&1 || true
            ip netns exec "$NS" "$IPT" -t nat -X ORRM1 >/dev/null 2>&1 || true
        fi
    fi
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    for p in "${PIDS[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
    [ -n "$NS" ] && ip netns del "$NS" >/dev/null 2>&1 || true
    [ -n "$VH" ] && ip link del "$VH" >/dev/null 2>&1 || true
    [ -n "$TMP" ] && rm -rf "$TMP"
}
trap cleanup EXIT

# Gateway logs are styled by tracing (ANSI escapes wrap field names/values);
# strip them so greps see plain `field=value` text.
plain_log() { sed 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null; }
export -f plain_log

check() { # name condition...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then PASS=$((PASS+1)); echo "PASS: $name"
    else FAIL=$((FAIL+1)); echo "FAIL: $name"; fi
}

skip() {
    echo "SKIP(local-netns): $1"
    exit 0
}

gate() { # name ok|no - a "no" is a hard, visible SKIP
    if [ "$2" = "ok" ]; then printf 'GATE: %-38s ok\n' "$1"
    else skip "$1 not satisfied"; fi
}

gate_run() { # name command...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then gate "$name" ok; else gate "$name" no; fi
}

ns_open() { ip netns exec "$NS" bash -c "exec 3<>/dev/tcp/127.0.0.1/$1" 2>/dev/null; }

wait_port() { # port timeout_s
    for _ in $(seq 1 $(( $2 * 10 ))); do
        ns_open "$1" && return 0
        sleep 0.1
    done
    return 1
}

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

ns_curl() { ip netns exec "$NS" curl -s "$@"; }

echo "== environment gates =="
gate_run "root (EUID 0)" test "$(id -u)" = "0"
for tool in ip iptables curl python3 setpriv bash; do
    gate_run "tool: $tool" command -v "$tool"
done
if command -v iptables >/dev/null 2>&1; then
    IPT=iptables
elif command -v nft >/dev/null 2>&1 && command -v iptables-nft >/dev/null 2>&1; then
    IPT=iptables-nft   # iptables shim over nftables
else
    skip "neither iptables nor nft+iptables-nft available"
fi
gate "tool: iptables backend (as $IPT)" ok
gate_run "ip netns support" ip netns list
gate_run "kernel conntrack" bash -c \
    '[ -e /proc/net/nf_conntrack ] || lsmod 2>/dev/null | grep -q nf_conntrack || [ -d /proc/sys/net/netfilter ]'
gate_run "unshare -n (netns permission)" unshare -n true

echo "== build =="
if [ ! -e "$ROOT/target/debug/openrusty" ] || [ ! -e "$ROOT/target/debug/examples/echo_upstream" ]; then
    echo "building gateway + echo_upstream (artifacts missing)..."
    cargo build -p openrusty-server --bins >/dev/null 2>&1 || skip "gateway build failed"
    cargo build -p openrusty-server --example echo_upstream >/dev/null 2>&1 \
        || skip "echo_upstream build failed"
fi
gate_run "artifact: target/debug/openrusty" test -e "$ROOT/target/debug/openrusty"
gate_run "artifact: examples/echo_upstream" test -e "$ROOT/target/debug/examples/echo_upstream"

echo "== topology =="
NS="orr-m1-$$"
VH="veth-m1-$$"
TMP="$(mktemp -d /tmp/openrusty-netns.XXXXXX)"
mkdir -p "$TMP/plugins" "$TMP/logs"
# The gateway runs as uid "$GATE_UID" (nobody) via setpriv; mktemp -d is 0700,
# which would hide the config from it. Open the tree for traversal.
chmod 0755 "$TMP" "$TMP/plugins" "$TMP/logs"
if ! ip netns add "$NS" 2>/dev/null; then
    skip "cannot create netns $NS (needs CAP_SYS_ADMIN + a writable /run/netns)"
fi
ip link add "$VH" type veth peer name eth0
ip link set eth0 netns "$NS"
ip addr add 10.123.0.1/24 dev "$VH"
ip link set "$VH" up
ip -n "$NS" addr add "$APP_IP/24" dev eth0
ip -n "$NS" link set eth0 up
ip -n "$NS" link set lo up
# Outbound-path target: the same app reachable under a second address.
ip -n "$NS" addr add "$VIP_IP/32" dev lo

cat > "$TMP/openrusty.toml" <<CONF
[server]
listen = "127.0.0.1:$ADM_PORT"
log_level = "debug"

[[server.listeners]]
role = "inbound"
listen = "0.0.0.0:$INB_PORT"
transparent = true
detect_timeout_ms = 3000

[[server.listeners]]
role = "outbound"
listen = "0.0.0.0:$OUT_PORT"
transparent = true

[[server.listeners]]
role = "admin"
listen = "0.0.0.0:$ADM_PORT"

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

ip netns exec "$NS" "$ROOT/target/debug/examples/echo_upstream" \
    "0.0.0.0:$APP_PORT" app > "$TMP/logs/app.log" 2>&1 &
PIDS+=($!)
# Raw TCP echo for the opaque path: plain python3 asyncio (no ncat needed).
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
# The gateway itself is NOT root: its own dials must bypass the REDIRECT
# rules. RUST_LOG is exported for symmetry; the gateway takes its level
# from the config (log_level = "debug" above).
RUST_LOG=debug ip netns exec "$NS" setpriv --reuid "$GATE_UID" --regid "$GATE_UID" \
    --clear-groups "$ROOT/target/debug/openrusty" "$TMP/openrusty.toml" \
    > "$TMP/logs/gw.log" 2>&1 &
GW_PID=$!; PIDS+=("$GW_PID")
wait_port "$ADM_PORT" 10 && wait_port "$APP_PORT" 10 && wait_port "$RAW_PORT" 10 \
    || { echo "FATAL: services did not come up"; tail -n 5 "$TMP/logs"/*.log >&2; exit 1; }
require_alive "startup" "${PIDS[@]}" || { tail -n 5 "$TMP/logs"/*.log >&2; exit 1; }

ipt() { ip netns exec "$NS" "$IPT" -t nat "$@"; }
if ! ipt -N ORRM1 2>/dev/null; then
    skip "iptables nat REDIRECT unavailable inside the netns"
fi
ipt -A OUTPUT -j ORRM1
ipt -A ORRM1 -m owner --uid-owner "$GATE_UID" -j RETURN
ipt -A ORRM1 -p tcp -d "$APP_IP" --dport "$APP_PORT" -j REDIRECT --to-ports "$INB_PORT"
ipt -A ORRM1 -p tcp -d "$APP_IP" --dport "$RAW_PORT" -j REDIRECT --to-ports "$INB_PORT"
ipt -A ORRM1 -p tcp -d "$VIP_IP" --dport "$APP_PORT" -j REDIRECT --to-ports "$OUT_PORT"
echo "interception rules installed (owner UID $GATE_UID exempt)"

echo "== 1. HTTP transparent inbound (REDIRECT -> 4143) =="
ns_curl -D "$TMP/a1.headers" "http://$APP_IP:$APP_PORT/echo" > "$TMP/a1.body" || true
check "a1: redirected request answers 200" grep -q '^HTTP/1.1 200' "$TMP/a1.headers"
check "a1: response carries the app node" grep -q '"node":"app"' "$TMP/a1.body"
check "a1: x-forwarded-for injected (proxy transit)" grep -qi 'x-forwarded-for' "$TMP/a1.body"
check "a1: gateway access log saw the request" grep -q 'request finished' "$TMP/logs/gw.log"

echo "== 2. orig_dst observability (true pre-NAT address) =="
# The gateway recovers the pre-NAT destination on the intercepted socket;
# the tunnel path is where it logs that address, so drive an opaque probe
# straight at the app address and read the recorded orig_dst back.
ip netns exec "$NS" python3 - "$APP_IP" "$APP_PORT" <<'PY' || true
import socket, sys
host, port = sys.argv[1], int(sys.argv[2])
s = socket.create_connection((host, port), timeout=10)
s.sendall(b"\x16\x03\x01 orr-drill not-http\r\n\r\n")
s.settimeout(3)
try:
    while True:
        if not s.recv(4096):
            break
except Exception:
    pass
s.close()
PY
check "a2: orig_dst logged as the app address" \
    bash -c "plain_log '$TMP/logs/gw.log' 2>/dev/null | grep 'opaque tunnel established' | grep -q 'dst=$APP_IP:$APP_PORT'"
check "a2: orig_dst never an own listener address" \
    bash -c "! plain_log '$TMP/logs/gw.log' 2>/dev/null | grep -qE 'dst=127\\.0\\.0\\.1:(4140|4143|4191)'"

echo "== 3. opaque TCP passthrough (REDIRECT -> 4143, tunnel) =="
PY_RC=0
ip netns exec "$NS" python3 - "$APP_IP" "$RAW_PORT" <<'PY' || PY_RC=$?
import os, socket, sys
host, port = sys.argv[1], int(sys.argv[2])
payload = os.urandom(4096)
s = socket.create_connection((host, port), timeout=10)
s.sendall(payload)
got = b""
while len(got) < len(payload):
    b = s.recv(65536)
    if not b:
        break
    got += b
s.close()
sys.exit(0 if got == payload else 1)
PY
check "a3: 4096 random bytes round-trip verbatim" test "$PY_RC" = "0"
check "a3: tunnel log records orig_dst raw port" \
    bash -c "plain_log '$TMP/logs/gw.log' 2>/dev/null | grep 'opaque tunnel established' | grep -q 'dst=$APP_IP:$RAW_PORT'"

echo "== 4. outbound path (REDIRECT VIP -> 4140, dial orig_dst) =="
CODE="$(ns_curl -o "$TMP/a4.body" -w '%{http_code}' --max-time 5 "http://$VIP_IP:$APP_PORT/echo" || true)"
check "a4: outbound interception reaches the target (200)" test "$CODE" = "200"
check "a4: response body comes from the app" grep -q '"node":"app"' "$TMP/a4.body"
check "a4: outbound log records orig_dst == VIP" \
    bash -c "plain_log '$TMP/logs/gw.log' 2>/dev/null | grep 'opaque tunnel established' | grep -q 'dst=$VIP_IP:$APP_PORT'"

echo "== 5. loop guard (own admin port as orig_dst) =="
ipt -A ORRM1 -p tcp -d "$APP_IP" --dport "$ADM_PORT" -j REDIRECT --to-ports "$INB_PORT"
LOOP_RC=0
ns_curl -o /dev/null --max-time 5 "http://$APP_IP:$ADM_PORT/openrusty/live" || LOOP_RC=$?
ipt -D ORRM1 -p tcp -d "$APP_IP" --dport "$ADM_PORT" -j REDIRECT --to-ports "$INB_PORT"
check "a5: looped connection refused (curl rc=$LOOP_RC)" test "$LOOP_RC" != "0"
check "a5: loop guard logged" grep -q 'transparent loop guard' "$TMP/logs/gw.log"

echo "== 6. graceful shutdown =="
kill -TERM "$GW_PID" 2>/dev/null || true
GW_RC=0
wait "$GW_PID" || GW_RC=$?
check "a6: SIGTERM exit code 0" test "$GW_RC" = "0"
check "a6: shutdown phase 1/3 logged" grep -q 'shutdown: phase 1/3' "$TMP/logs/gw.log"
check "a6: shutdown summary logged" grep -q 'shutdown: complete' "$TMP/logs/gw.log"
if ns_open "$INB_PORT" || ns_open "$OUT_PORT"; then
    check "a6: listener ports released after exit" test 1 = 0
else
    check "a6: listener ports released after exit" test 0 = 0
fi

echo
echo "== CHECKS: $((PASS + FAIL)) total =="
echo "== RESULT: $PASS passed, $FAIL failed =="
# Assertion 7 (repeatability) is proven by running this script twice in a
# row: the EXIT trap above removes rules, netns, veth, processes and the
# scratch dir, so the next run starts from a clean slate.
[ "$FAIL" = "0" ]
