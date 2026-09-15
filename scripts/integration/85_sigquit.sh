# integration drill section 85: SIGQUIT fast shutdown (nginx QUIT) - with
# a request parked on a never-responding upstream, QUIT stops accepting,
# skips the 3s drain, and exits 0 promptly; the summary reports the
# in-flight connection as force-closed (forced >= 1).
SIGQUIT_PORT="${SIGQUIT_PORT:-18198}"
HANG_PORT="${HANG_PORT:-18195}"

echo "== 85. SIGQUIT fast shutdown (drain skipped) =="
mkdir -p "$TMP/sigquit/plugins"
# Upstream that accepts the TCP connection and never answers.
python3 -c "
import socket
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', $HANG_PORT))
s.listen(16)
held = []
while True:
    conn, _ = s.accept()
    held.append(conn)
" > "$TMP/sigquit/hang-up.log" 2>&1 &
PIDS+=($!)
cat > "$TMP/sigquit/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$SIGQUIT_PORT"
log_level = "info"
shutdown_grace_ms = 3000
[plugins]
dir = "$TMP/sigquit/plugins"
[[upstreams]]
name = "hang-up"
  [[upstreams.peers]]
  addr = "127.0.0.1:$HANG_PORT"
[[routes]]
path_prefix = "/hang"
upstream = "hang-up"
timeout_ms = 8000
CONF
"$ROOT/target/debug/openrusty" "$TMP/sigquit/gw.toml" > "$TMP/sigquit/gw.log" 2>&1 &
SIGQUIT_PID=$!
PIDS+=($SIGQUIT_PID)
wait_port $SIGQUIT_PORT 10 || { echo "FATAL: sigquit gateway did not come up"; exit 1; }

# A request parked on the silent upstream (its curl erroring out later is
# expected and never counted); give it a moment to become in-flight.
curl -s -o "$TMP/sigquit/hang.out" --max-time 8 \
    "http://127.0.0.1:$SIGQUIT_PORT/hang" &
PIDS+=($!)

sleep 0.5
QUIT_T0=$(date +%s%3N)
kill -QUIT "$SIGQUIT_PID"
SIGQUIT_LATE=no
while kill -0 "$SIGQUIT_PID" 2>/dev/null; do
    if [ "$(date +%s%3N)" -ge $((QUIT_T0 + 2500)) ]; then SIGQUIT_LATE=yes; break; fi
    sleep 0.02
done
QUIT_ELAPSED=$(( $(date +%s%3N) - QUIT_T0 ))
[ "$SIGQUIT_LATE" = yes ] && kill -9 "$SIGQUIT_PID" 2>/dev/null || true
SIGQUIT_RC=0
wait "$SIGQUIT_PID" || SIGQUIT_RC=$?
# tracing writes ANSI field decoration; strip it so greps stay plain.
sed 's/\x1b\[[0-9;]*m//g' "$TMP/sigquit/gw.log" > "$TMP/sigquit/gw.plain"

check "SIGQUIT: gateway exits within 2s (drain skipped)" \
    bash -c "test '$SIGQUIT_LATE' = no && test $QUIT_ELAPSED -le 2000"
check "SIGQUIT: exit code is 0" test "$SIGQUIT_RC" = "0"
check "SIGQUIT: log shows skipped drain and forced in-flight conn" \
    bash -c "grep -q 'skipping drain' '$TMP/sigquit/gw.plain' \
             && grep -Eq 'forced=[1-9][0-9]*' '$TMP/sigquit/gw.plain'"
