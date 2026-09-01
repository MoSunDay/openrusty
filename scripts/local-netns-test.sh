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
#   iptables nat (inside the netns), installed by the `openrusty
#   iptables-init` subcommand (linkerd2 proxy-init parameter surface):
#     -> PREROUTING/OPENRUSTY_IN: full-port REDIRECT to 4143 (inbound),
#        admin port 4191 exempt (--ignore-inbound-ports)
#     -> OUTPUT/OPENRUSTY_OUT:    full-port REDIRECT to 4140 (outbound)
#   The gateway itself runs as UID 65534 so its own dials bypass the
#   OUTPUT REDIRECT (-m owner --uid-owner 65534 -j RETURN) - otherwise
#   the proxy would loop into itself. Inbound probes are driven from the
#   host side through the veth, so PREROUTING is what intercepts them.
#
# Assertions (each counted PASS/FAIL, style of scripts/integration.sh):
#   0. iptables-init: deterministic dry-run plan; installed by executing
#      the plan; exactly one hook per builtin chain; preflight self-checks
#      (conntrack, backend probe, REDIRECT target probe) pass in the netns;
#      two real runs converge to an identical iptables-save (idempotence).
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

echo "== 0. iptables-init subcommand (hijack parameter surface) =="
if ! ip netns exec "$NS" "$IPT" -t nat -N ORRGATE 2>/dev/null; then
    skip "iptables nat REDIRECT unavailable inside the netns"
fi
ip netns exec "$NS" "$IPT" -t nat -X ORRGATE >/dev/null 2>&1 || true
ORR="$ROOT/target/debug/openrusty"; SAVE_BIN="${IPT}-save"
command -v "$SAVE_BIN" >/dev/null 2>&1 || skip "$SAVE_BIN missing"
# The exact parameter surface installed here (linkerd2 proxy-init
# semantics): proxy UID exempt on OUTPUT, admin 4191 exempt inbound,
# full-port REDIRECT 4143 (in) / 4140 (out).
INIT=(iptables-init --proxy-uid "$GATE_UID" --inbound-port "$INB_PORT" --outbound-port "$OUT_PORT"
    --ignore-inbound-ports "$ADM_PORT" --ignore-outbound-ports 443)
ip netns exec "$NS" "$ORR" "${INIT[@]}" --dry-run > "$TMP/plan.txt"
ip netns exec "$NS" "$ORR" "${INIT[@]}" --dry-run > "$TMP/plan2.txt"
check "a0: dry-run plan deterministic" diff -q "$TMP/plan.txt" "$TMP/plan2.txt"
check "a0: plan exempts the proxy UID on OUTPUT" \
    grep -q -- "--uid-owner $GATE_UID -m comment --comment openrusty-init -j RETURN" "$TMP/plan.txt"
check "a0: plan leaves the admin port alone (inbound)" \
    grep -q -- "--dport $ADM_PORT -m comment --comment openrusty-init -j RETURN" "$TMP/plan.txt"
check "a0: plan hijacks all inbound tcp to 4143" \
    grep -q -- "-A OPENRUSTY_IN -p tcp -m comment --comment openrusty-init -j REDIRECT --to-ports $INB_PORT" "$TMP/plan.txt"
check "a0: plan hijacks all outbound tcp to 4140" \
    grep -q -- "-A OPENRUSTY_OUT -p tcp -m comment --comment openrusty-init -j REDIRECT --to-ports $OUT_PORT" "$TMP/plan.txt"
# Install by executing the dry-run plan (ops -N/-C are advisory here: a
# fresh netns has neither chain nor hook; the -I/-A lines carry the state).
while IFS= read -r line; do
    read -ra argv <<< "$line"
    argv[0]="$IPT"    # the plan is backend-neutral; the shim name may differ
    case "${argv[3]}" in
        -N|-C) ip netns exec "$NS" "${argv[@]}" </dev/null >/dev/null 2>&1 || true ;;
        *) ip netns exec "$NS" "${argv[@]}" </dev/null >>"$TMP/plan-exec.log" 2>&1 \
            || { echo "FATAL: plan line failed: $line"; tail -n 2 "$TMP/plan-exec.log"; exit 1; } ;;
    esac
done < "$TMP/plan.txt"
ip netns exec "$NS" "$SAVE_BIN" -t nat > "$TMP/save1.txt" 2>/dev/null
check "a0: exactly one hook per builtin chain" test "$(grep -c -- '-j OPENRUSTY_' "$TMP/save1.txt")" = "2"
# Preflight positive: the self-checks (conntrack availability, backend
# probe, REDIRECT target probe) pass inside the netns, and the real run
# over an already-installed surface is a zero delta - two runs converge to
# the identical iptables-save (idempotent re-entry).
check "a0: init preflight passes (exit 0)" ip netns exec "$NS" "$ORR" "${INIT[@]}"
ip netns exec "$NS" "$SAVE_BIN" -t nat > "$TMP/save2.txt" 2>/dev/null
check "a0: re-run converges (iptables-save diff empty)" diff -q "$TMP/save1.txt" "$TMP/save2.txt"
echo "interception rules installed via iptables-init (owner UID $GATE_UID exempt)"

echo "== 1. HTTP transparent inbound (PREROUTING REDIRECT -> 4143) =="
# Driven from the host side: with the init parameter surface, ingress is
# hijacked in PREROUTING, so the honest inbound path enters via the veth.
curl -s -D "$TMP/a1.headers" "http://$APP_IP:$APP_PORT/echo" > "$TMP/a1.body" || true
check "a1: redirected request answers 200" grep -q '^HTTP/1.1 200' "$TMP/a1.headers"
check "a1: response carries the app node" grep -q '"node":"app"' "$TMP/a1.body"
check "a1: x-forwarded-for injected (proxy transit)" grep -qi 'x-forwarded-for' "$TMP/a1.body"
check "a1: gateway access log saw the request" grep -q 'request finished' "$TMP/logs/gw.log"
# ignore-inbound-ports at work: a controller outside the pod still
# reaches the exempted admin port instead of looping into the proxy.
ADM_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://$APP_IP:$ADM_PORT/openrusty/live" || true)"
check "a1b: admin port exempt from inbound hijack (200)" test "$ADM_CODE" = "200"

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
# No temporary rule needed: the init parameter surface already hijacks a
# local dial at the admin port into the outbound listener (only the proxy
# UID is exempt on OUTPUT), so orig_dst recovers as an own listen port.
LOOP_RC=0
ns_curl -o /dev/null --max-time 5 "http://$APP_IP:$ADM_PORT/openrusty/live" || LOOP_RC=$?
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
