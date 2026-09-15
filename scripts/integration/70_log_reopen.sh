# integration drill section 70: server.log_file rotation - logs append to
# the configured file (stdout silent), SIGUSR1 reopens the path after an
# external rename (the logrotate contract), the admin plane keeps serving
# across the reopen, the reopen marker lands in the NEW file only, and a
# graceful SIGTERM still exits 0.

LOGREOPEN_PORT="${LOGREOPEN_PORT:-18190}"

echo "== 70. log file + USR1 reopen (service uninterrupted) =="
mkdir -p "$TMP/logreopen/plugins"
cat > "$TMP/logreopen/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$LOGREOPEN_PORT"
log_level = "info"
log_file = "$TMP/logreopen/gw.log"
shutdown_grace_ms = 2000

[plugins]
dir = "$TMP/logreopen/plugins"
order = []
CONF
"$ROOT/target/debug/openrusty" "$TMP/logreopen/gw.toml" \
    > "$TMP/logreopen/stdout.log" 2>&1 &
LOGREOPEN_PID=$!
PIDS+=($LOGREOPEN_PID)
wait_port $LOGREOPEN_PORT 10 \
    || { echo "FATAL: log-reopen gateway did not come up"; exit 1; }

PRE_CODE="$(code_of 5 "http://127.0.0.1:$LOGREOPEN_PORT/openrusty/live")"
check "log file: gateway serves /openrusty/live before rotation" \
    test "$PRE_CODE" = "200"

# logrotate semantics: rename the file out from under the gateway, then
# signal; it re-creates and re-opens the same path in append mode.
mv "$TMP/logreopen/gw.log" "$TMP/logreopen/gw.old.log"
kill -USR1 "$LOGREOPEN_PID"
sleep 0.5 # reopen runs on the signal task; let it settle before probing
POST_CODE="$(code_of 5 "http://127.0.0.1:$LOGREOPEN_PORT/openrusty/live")"
sleep 0.3 # the writer is async-buffered; give the marker time to flush
check "log file: gateway still serves after USR1 reopen" test "$POST_CODE" = "200"
check "log file: 'log file reopened' marker lands in the NEW file" \
    grep -q "log file reopened" "$TMP/logreopen/gw.log"
check "log file: rotated file holds pre-reopen history only (marker absent)" \
    bash -c "test -s '$TMP/logreopen/gw.old.log' \
             && ! grep -q 'log file reopened' '$TMP/logreopen/gw.old.log'"

kill -TERM "$LOGREOPEN_PID"
LOGREOPEN_RC=0
wait "$LOGREOPEN_PID" || LOGREOPEN_RC=$?
# The pid is reaped; cleanup's later kill of the dead pid is a no-op.
check "log file: SIGTERM exits 0" test "$LOGREOPEN_RC" = "0"
