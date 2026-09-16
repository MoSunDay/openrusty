# integration drill section 90: management-plane token auth - with
# [admin] token set every /openrusty/* endpoint except the ready/live
# probes requires the shared secret (Bearer or X-OpenRusty-Token);
# without the section the plane stays exactly as open as before
# (backward compat), the data plane never needs the token, and reload
# still enforces the loopback rule on top of a valid token.
ADMINAUTH_PORT="${ADMINAUTH_PORT:-18200}"
ADMINAUTH="http://127.0.0.1:$ADMINAUTH_PORT"

echo "== 33. admin plane token auth =="
mkdir -p "$TMP/adminauth"
cat > "$TMP/adminauth/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$ADMINAUTH_PORT"
log_level = "info"

[plugins]
dir = "$TMP/plugins"
order = []

[admin]
token = "drill-admin-token-1"

[[upstreams]]
name = "u"
  [[upstreams.peers]]
  addr = "127.0.0.1:19101"

[[routes]]
path_prefix = "/"
upstream = "u"
timeout_ms = 5000
CONF
"$ROOT/target/debug/openrusty" "$TMP/adminauth/gw.toml" >> "$TMP/logs/adminauth.log" 2>&1 &
ADMINAUTH_PID=$!
PIDS+=($ADMINAUTH_PID)
wait_port $ADMINAUTH_PORT 10 \
    || { echo "FATAL: admin-auth gateway did not come up"; exit 1; }

# Backward compat: the main drill gateway has no [admin] section, so its
# management plane answers unauthenticated exactly as before.
check "auth off by default: main gateway status stays open" \
    test "$(code_of 5 "$GATE/openrusty/status")" = "200"

AUTH_CODE() { # curl args...
    curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$@"
}
check "token gateway: status without token is 401" \
    test "$(AUTH_CODE "$ADMINAUTH/openrusty/status")" = "401"
check "token gateway: wrong bearer is 401" \
    test "$(AUTH_CODE -H "Authorization: Bearer nope" "$ADMINAUTH/openrusty/status")" = "401"
check "token gateway: X-OpenRusty-Token is accepted" \
    test "$(AUTH_CODE -H "X-OpenRusty-Token: drill-admin-token-1" "$ADMINAUTH/openrusty/status")" = "200"
check "token gateway: bearer is accepted" \
    bash -c "curl -s --max-time 5 -H 'Authorization: Bearer drill-admin-token-1' \
             '$ADMINAUTH/openrusty/status' | grep -q '\"generation\"'"
check "token gateway: ready probe stays open" \
    test "$(AUTH_CODE "$ADMINAUTH/openrusty/ready")" = "200"
check "token gateway: live probe stays open" \
    test "$(AUTH_CODE "$ADMINAUTH/openrusty/live")" = "200"
check "token gateway: metrics without token is 401" \
    test "$(AUTH_CODE "$ADMINAUTH/openrusty/metrics")" = "401"
check "token gateway: reload without token is 401" \
    test "$(AUTH_CODE -X POST "$ADMINAUTH/openrusty/reload")" = "401"
check "token gateway: reload with token succeeds" \
    bash -c "curl -s -X POST --max-time 10 \
             -H 'Authorization: Bearer drill-admin-token-1' \
             '$ADMINAUTH/openrusty/reload' | grep -q '\"generation\"'"
# The token guards the management plane only: proxied traffic never asks
# for it (the catch-all route hits the echo upstream from section 00).
check "token gateway: data plane needs no token" \
    bash -c "curl -s --max-time 5 '$ADMINAUTH/' | grep -q 'node'"

check "token gateway: shutdown with token" \
    test "$(AUTH_CODE -X POST -H "X-OpenRusty-Token: drill-admin-token-1" \
        "$ADMINAUTH/openrusty/shutdown")" = "200"
ADMINAUTH_GONE=no
for _ in $(seq 1 50); do
    if ! kill -0 "$ADMINAUTH_PID" 2>/dev/null; then ADMINAUTH_GONE=yes; break; fi
    sleep 0.1
done
wait "$ADMINAUTH_PID" 2>/dev/null || true
check "token gateway: shutdown actually stops the process" \
    test "$ADMINAUTH_GONE" = "yes"
