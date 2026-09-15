# integration drill section 80: systemd socket activation - the gateway
# inherits a pre-bound listening socket as fd 3 (LISTEN_FDS=1 + exact
# LISTEN_PID via fork+exec); a SIGTERM restart mid-load never refuses a
# connection because the harness keeps the socket open across the respawn.
SOCKACT_PORT="${SOCKACT_PORT:-18197}"
SOCKACT_UP_PORT="${SOCKACT_UP_PORT:-18196}"

echo "== 80. socket activation: zero-refusal restart under load =="
mkdir -p "$TMP/sockact/plugins"
cat > "$TMP/sockact/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$SOCKACT_PORT"
log_level = "info"
shutdown_grace_ms = 3000
[plugins]
dir = "$TMP/sockact/plugins"
[[upstreams]]
name = "sock-up"
  [[upstreams.peers]]
  addr = "127.0.0.1:$SOCKACT_UP_PORT"
[[routes]]
path_prefix = "/echo"
upstream = "sock-up"
timeout_ms = 4000
CONF

# Self-contained harness: threaded HTTP upstream, the activation socket
# (held open for the whole run), 4x50 request load, mid-load SIGTERM +
# respawn on the same socket, full child reaping.
cat > "$TMP/sockact/harness.py" <<'PY'
import http.client
import http.server
import os
import socket
import sys
import threading
import time

PORT, UP, GATEWAY, CONFIG, OUTPREFIX = (
    int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5])
THREADS, PER_THREAD, RESTART_AFTER = 4, 50, 100


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"ok":true}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


upstream = http.server.ThreadingHTTPServer(("127.0.0.1", UP), Handler)
threading.Thread(target=upstream.serve_forever, daemon=True).start()

# Activation socket: bound here, never closed - both spawns inherit it
# as fd 3, and connects queue in its backlog between them.
lsock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
lsock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
lsock.bind(("127.0.0.1", PORT))
lsock.listen(128)


def spawn(idx):
    # fork+exec so LISTEN_PID is the gateway's own pid; dup2 lands the
    # listening socket on fd 3 (the sd_listen_fds(3) convention).
    out = open("%s.gw%d.out" % (OUTPREFIX, idx), "ab")
    pid = os.fork()
    if pid == 0:
        os.dup2(lsock.fileno(), 3)
        os.dup2(out.fileno(), 1)
        os.dup2(out.fileno(), 2)
        env = dict(os.environ)
        env["LISTEN_FDS"] = "1"
        env["LISTEN_PID"] = str(os.getpid())
        os.execve(GATEWAY, [GATEWAY, CONFIG], env)
        os._exit(127)
    return pid


stats = {"completed": 0, "connect_errors": 0, "http_errors": 0}
lock = threading.Lock()


def loader():
    for _ in range(PER_THREAD):
        conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=5)
        try:
            conn.request("GET", "/echo")
            resp = conn.getresponse()
            resp.read()
            bucket = "completed" if 200 <= resp.status < 300 else "http_errors"
        except Exception:
            bucket = "connect_errors"
        finally:
            conn.close()
        with lock:
            stats[bucket] += 1
        time.sleep(0.01)

exits = []
gw = None
try:
    gw = spawn(1)
    threads = [threading.Thread(target=loader) for _ in range(THREADS)]
    for t in threads:
        t.start()
    # Restart mid-load: SIGTERM the gateway (graceful 3s drain) and
    # respawn immediately on the SAME socket while loaders keep going.
    while True:
        with lock:
            if stats["completed"] + stats["http_errors"] >= RESTART_AFTER:
                break
        if not any(t.is_alive() for t in threads):
            break
        time.sleep(0.005)
    os.kill(gw, 15)
    exits.append(os.waitstatus_to_exitcode(os.waitpid(gw, 0)[1]))
    gw = spawn(2)
    for t in threads:
        t.join()
    os.kill(gw, 15)
    exits.append(os.waitstatus_to_exitcode(os.waitpid(gw, 0)[1]))
    gw = None
finally:
    if gw is not None:  # a failed run must not leak a gateway child
        try:
            os.kill(gw, 9)
            os.waitpid(gw, 0)
        except OSError:
            pass
print("SOCKACT gw_exits=%s" % ",".join(str(e) for e in exits))
print("SOCKACT completed=%d connect_errors=%d http_errors=%d"
      % (stats["completed"], stats["connect_errors"], stats["http_errors"]))
PY
if ! timeout 60 python3 "$TMP/sockact/harness.py" "$SOCKACT_PORT" "$SOCKACT_UP_PORT" "$ROOT/target/debug/openrusty" "$TMP/sockact/gw.toml" "$TMP/sockact/out" > "$TMP/sockact/result.txt" 2>&1; then
    echo "FATAL: socket-activation harness failed" >&2
    cat "$TMP/sockact/result.txt" >&2
    exit 1
fi
tail -2 "$TMP/sockact/result.txt"
SOCKACT_INHERITED="$(grep -h 'listener inherited' "$TMP"/sockact/out.gw*.out 2>/dev/null | wc -l)"
check "socket activation: full load completes (200/200 requests)" grep -q 'completed=200' "$TMP/sockact/result.txt"
check "socket activation: zero connect errors across the restart" grep -q 'connect_errors=0' "$TMP/sockact/result.txt"
check "socket activation: zero http errors across the restart" grep -q 'http_errors=0' "$TMP/sockact/result.txt"
check "socket activation: both spawns inherited fd 3" test "$SOCKACT_INHERITED" -ge 2
check "socket activation: both gateways exited 0 on SIGTERM" grep -q 'gw_exits=0,0' "$TMP/sockact/result.txt"
