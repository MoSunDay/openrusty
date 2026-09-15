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
# header passthrough, ip_hash dead-node failover, the concurrent
# reload race (in-flight reloads are rejected with 409), the dynamic
# WASM API (POST /api/v1/dynamic/{name}: settings plumbing, module
# headers, error codes, body cap, replace-without-reload, warm cache
# across reloads), upstream TLS (https peers: CA pinning, wrong-CA
# rejection, insecure skip), log-file rotation with SIGUSR1 reopen
# (service uninterrupted across the rotate), systemd socket activation
# (fd-3 inheritance, zero-refusal restart under load), and SIGQUIT fast
# shutdown (drain skipped, in-flight forced).
# The drill body lives in scripts/integration/*.sh, sourced below in
# execution order; fragments run in this shell and share the globals
# and helpers defined here.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
TMP="$(mktemp -d /tmp/openrusty-it.XXXXXX)"
GATE_PORT="${GATE_PORT:-18080}"
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

# §31 (dynamic wasm API) lives in its own library so this script stays
# under the size cap; it shares the check/GATE/TMP/ROOT globals above.
. "$(dirname "$0")/lib-dynamic-drill.sh"

# ---- drill sections (order matters; keep in sync with the numbers) ----
. "$(dirname "$0")/integration/00_setup_gateways.sh"
. "$(dirname "$0")/integration/10_proxy_basics.sh"
. "$(dirname "$0")/integration/20_reload_health.sh"
. "$(dirname "$0")/integration/30_plugins_probe.sh"
. "$(dirname "$0")/integration/40_env_extract_active.sh"
. "$(dirname "$0")/integration/50_retry_metrics_race.sh"
. "$(dirname "$0")/integration/60_dynamic_tls.sh"
. "$(dirname "$0")/integration/70_log_reopen.sh"
. "$(dirname "$0")/integration/80_socket_activation.sh"
. "$(dirname "$0")/integration/85_sigquit.sh"


echo
echo "== CHECKS: $((PASS + FAIL)) total =="
echo "== RESULT: $PASS passed, $FAIL failed =="
[ "$FAIL" = "0" ]
