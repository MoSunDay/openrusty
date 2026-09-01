#!/usr/bin/env bash
# Shared helpers for the local netns drills (scripts/local-netns-test.sh and
# scripts/local-egress-test.sh). This file is a library: SOURCE it, never
# execute it. It intentionally knows nothing about the drills' topology -
# each drill declares its own variables (NS, VH, TMP, PIDS, PASS, FAIL,
# IPT, SKIP_TAG, optional PROBE_UID) and sources this file afterwards.
#
# Everything here talks to one throwaway network namespace ("netns") and
# treats it as disposable: teardown is always safe to run twice.

# Only meaningful when executed: point at the library, don't run it.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    echo "lib-netns.sh is a library; source it from a drill script" >&2
    exit 64
fi

# UID the gateway runs as (nobody). Its own dials must bypass the OUTPUT
# REDIRECT (-m owner --uid-owner $GATE_UID -j RETURN) or the proxy would
# loop into itself. Drills may override before sourcing.
GATE_UID="${GATE_UID:-65534}"
# When set (e.g. to $GATE_UID), ns_open probes ports AS that UID so the
# probe itself is exempt from interception. Drills that install their
# iptables rules before a restart need this to re-probe the proxy ports.
# Empty = probe as root (historical behaviour of local-netns-test.sh).
PROBE_UID="${PROBE_UID:-}"

# Stop the drill with a visible, non-failing SKIP line (CI treats a skip
# as neutral: an environment gap, not a regression). The tag names the
# drill so a skipped matrix stays readable.
skip() {
    echo "SKIP(${SKIP_TAG:-netns}): $1"
    exit 0
}

netns_teardown() {
    for p in ${PIDS[@]:-}; do kill "$p" 2>/dev/null || true; done
    sleep 0.3
    for p in ${PIDS[@]:-}; do kill -9 "$p" 2>/dev/null || true; done
    [ -n "$NS" ] && ip netns del "$NS" >/dev/null 2>&1 || true
    [ -n "$VH" ] && ip link del "$VH" >/dev/null 2>&1 || true
    [ -n "$TMP" ] && rm -rf "$TMP"
}

# Gateway logs are styled by tracing (ANSI escapes wrap field names/values);
# strip them so greps see plain `field=value` text.
plain_log() { sed 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null; }
export -f plain_log

check() { # name condition...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then PASS=$((PASS + 1)); echo "PASS: $name"
    else FAIL=$((FAIL + 1)); echo "FAIL: $name"; fi
}

gate() { # name ok|no - a "no" is a hard, visible SKIP
    if [ "$2" = "ok" ]; then printf 'GATE: %-38s ok\n' "$1"
    else skip "$1 not satisfied"; fi
}

gate_run() { # name command...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then gate "$name" ok; else gate "$name" no; fi
}

ns_open() {
    if [ -n "$PROBE_UID" ]; then
        ip netns exec "$NS" setpriv --reuid "$PROBE_UID" --regid "$PROBE_UID" \
            --clear-groups bash -c "exec 3<>/dev/tcp/127.0.0.1/$1" 2>/dev/null
    else
        ip netns exec "$NS" bash -c "exec 3<>/dev/tcp/127.0.0.1/$1" 2>/dev/null
    fi
}

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

# Environment gates every drill needs: root, the toolset, and the kernel
# features netns + REDIRECT are built on. Sets IPT to the usable iptables
# backend (or skips when none exists). On any gap the drill SKIPS whole.
env_gates() {
    echo "== environment gates =="
    gate_run "root (EUID 0)" test "$(id -u)" = "0"
    local tool
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
}

# Build the gateway binary and the echo workload unless artifacts are
# already current. A failed build SKIPS (not FAIL): the drill measures the
# transparent plane, not the compiler.
build_artifacts() {
    echo "== build =="
    if [ ! -e "$ROOT/target/debug/openrusty" ] || [ ! -e "$ROOT/target/debug/examples/echo_upstream" ]; then
        echo "building gateway + echo_upstream (artifacts missing)..."
        cargo build -p openrusty-server --bins >/dev/null 2>&1 || skip "gateway build failed"
        cargo build -p openrusty-server --example echo_upstream >/dev/null 2>&1 \
            || skip "echo_upstream build failed"
    fi
    gate_run "artifact: target/debug/openrusty" test -e "$ROOT/target/debug/openrusty"
    gate_run "artifact: examples/echo_upstream" test -e "$ROOT/target/debug/examples/echo_upstream"
}

# Create the netns and wire it to the host through a veth. On failure the
# drill SKIPS: namespace creation needs CAP_SYS_ADMIN plus /run/netns.
#  APP_IP    e.g. 10.123.0.2   -> host veth 10.123.0.1/24, eth0 .2/24
#  VIP_IP    loopback VIP added inside the netns (outbound-path target)
netns_up() { # APP_IP VIP_IP
    local net_base="${1%.*}"
    echo "== topology =="
    # Host-side veth names are limited to IFNAMSIZ (15 chars): keep the
    # tag short. The netns name is just a file under /run/netns - any
    # length is fine there.
    NS="orr-${NET_TAG:-drill}-$$"
    VH="veth-${NET_TAG:0:3}-$$"
    TMP="$(mktemp -d /tmp/openrusty-netns.XXXXXX)"
    mkdir -p "$TMP/plugins" "$TMP/logs"
    # The gateways run as uid GATE_UID via setpriv; mktemp -d is 0700, which
    # would hide the configs from them. Open the tree for traversal.
    chmod 0755 "$TMP" "$TMP/plugins" "$TMP/logs"
    if ! ip netns add "$NS" 2>/dev/null; then
        skip "cannot create netns $NS (needs CAP_SYS_ADMIN + a writable /run/netns)"
    fi
    ip link add "$VH" type veth peer name eth0
    ip link set eth0 netns "$NS"
    ip addr add "$net_base.1/24" dev "$VH"
    ip link set "$VH" up
    ip -n "$NS" addr add "$net_base.2/24" dev eth0
    ip -n "$NS" link set eth0 up
    ip -n "$NS" link set lo up
    # Outbound-path target: the same workload reachable under a second
    # address (loopback VIP inside the netns).
    ip -n "$NS" addr add "$2/32" dev lo
}
