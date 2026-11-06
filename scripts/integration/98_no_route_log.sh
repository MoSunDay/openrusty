# integration drill section 98: log_on_no_route - with
# [plugins] log_on_no_route = true the plugin log phase runs for
# requests that never routed (404), matching nginx's always-on log
# phase. The main drill gateway has a catch-all `/` route (every path
# proxies), so a dedicated gateway with no catch-all is spun up here:
# it loads only kv-probe, and an unrouted `?l404=1&lmark=1` request
# must 404 AND leave the plugin log-phase markers behind:
# - mode=logcheck flips 409 -> 204 (probe:log_ran written by the log
#   phase of the unrouted request - the only lmark request here);
# - the plugin's host_log line lands in the gateway log.
# The kv-probe l404check mode short-circuits with a body-less Done
# while echoing the marker into its X-Log404 response header; the
# gateway applies plugin resp headers on body-less Done short-
# circuits too (docs/wasm-abi.md), so the header reaches the client
# and is asserted alongside the mode's own 204. Own port + own TOML
# under $TMP (§96 config-surface pattern).
NR_PORT="${NR_PORT:-18240}"

echo "== 98. log_on_no_route: plugin log phase on unrouted 404 =="
mkdir -p "$TMP/noroute/plugins"
cp build/plugins/kv-probe.wasm "$TMP/noroute/plugins/"
cat > "$TMP/noroute/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$NR_PORT"
log_level = "info"

[plugins]
dir = "$TMP/noroute/plugins"
order = ["kv-probe"]
timeout_ms = 250
max_memory_mb = 16
log_on_no_route = true

[[upstreams]]
name = "echo-uw"
  [[upstreams.peers]]
  addr = "127.0.0.1:19101"

[[routes]]
path_prefix = "/probe"
upstream = "echo-uw"
timeout_ms = 5000
CONF
"$ROOT/target/debug/openrusty" "$TMP/noroute/gw.toml" > "$TMP/noroute/gw.log" 2>&1 &
NR_PID=$!
PIDS+=($NR_PID)
wait_port $NR_PORT 10 || { echo "FATAL: no-route gateway did not come up"; exit 1; }
if ! require_alive "no-route gateway" "$NR_PID"; then
    tail -5 "$TMP/noroute/gw.log"
    exit 1
fi
NR_GATE="http://127.0.0.1:$NR_PORT"

CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$NR_GATE/probe?mode=logcheck" || true)"
check "no-route log: log marker absent before the unrouted request (409)" test "$CODE" = "409"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 \
    "$NR_GATE/definitely-no-such-route?l404=1&lmark=1" || true)"
check "no-route log: unrouted request answers 404" test "$CODE" = "404"
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$NR_GATE/probe?mode=logcheck" || true)"
check "no-route log: log phase ran on the unrouted request" test "$CODE" = "204"
check "no-route log: plugin log line from the unrouted request in gw log" \
    grep -q 'kv-probe log phase marker' "$TMP/noroute/gw.log"
CODE="$(curl -s -o /dev/null -D "$TMP/l404.hdr" -w '%{http_code}' --max-time 8 \
    "$NR_GATE/probe?mode=l404check" || true)"
check "no-route log: l404check probe short-circuits (204)" test "$CODE" = "204"
check "no-route log: X-Log404 response header echoes the marker (1)" \
    grep -qi '^x-log404: 1' "$TMP/l404.hdr"
