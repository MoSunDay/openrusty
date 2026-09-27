# integration drill section 88: graceful drain (SIGTERM) - with a request
# parked on a never-answering upstream, TERM stops accepting and waits out
# the full grace window (the §85 SIGQUIT contrast skips it). The listening
# socket closes at once, so the draining answers are observable on an
# EXISTING h2c connection: hyper's h2 graceful shutdown sends GOAWAY with
# last_stream_id=MAX and new streams opened inside that 1-RTT window are
# still dispatched - so /openrusty/ready answers 503 {"status":"draining"}
# while /openrusty/live stays 200, then the process exits 0 after grace.
DRAIN_PORT="${DRAIN_PORT:-18230}"
DRAIN_HANG_PORT="${DRAIN_HANG_PORT:-18231}"

echo "== 88. graceful drain: ready 503 draining, live 200, full grace =="
mkdir -p "$TMP/drain/plugins"
# Upstream that accepts the TCP connection and never answers.
python3 -c "
import socket
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', $DRAIN_HANG_PORT))
s.listen(16)
held = []
while True:
    conn, _ = s.accept()
    held.append(conn)
" > "$TMP/drain/hang-up.log" 2>&1 &
PIDS+=($!)
cat > "$TMP/drain/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$DRAIN_PORT"
log_level = "info"
shutdown_grace_ms = 8000

[plugins]
dir = "$TMP/drain/plugins"

[[upstreams]]
name = "hang-up"
  [[upstreams.peers]]
  addr = "127.0.0.1:$DRAIN_HANG_PORT"

[[routes]]
path_prefix = "/hang"
upstream = "hang-up"
timeout_ms = 20000
CONF
"$ROOT/target/debug/openrusty" "$TMP/drain/gw.toml" > "$TMP/drain/gw.log" 2>&1 &
DRAIN_PID=$!
PIDS+=($DRAIN_PID)
wait_port $DRAIN_PORT 10 || { echo "FATAL: drain gateway did not come up"; exit 1; }

# Minimal raw h2c client (stdlib only): opens a prior-knowledge h2
# connection, parks stream 1 on the hang route (keeps the connection
# in-flight through the drain), and when the drain's first GOAWAY arrives
# opens /openrusty/ready (stream 3) and /openrusty/live (stream 5) BEFORE
# answering the shutdown ping, so both streams are accepted ahead of the
# final GOAWAY. Prints "READY <code> <body>" and "LIVE <code> <body>".
rm -f "$TMP/drain/probe.out"
python3 - "$DRAIN_PORT" "$TMP/drain/probe.out" <<'PY' > "$TMP/drain/probe.err" 2>&1 &
import socket
import sys
import time

PORT, OUT = int(sys.argv[1]), sys.argv[2]
PREFACE = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
STATIC_STATUS = {8: "200", 9: "204", 10: "206", 11: "304",
                 12: "400", 13: "404", 14: "500"}
# Canonical-huffman digit codes (RFC 7541 App. B): no digit code is a
# prefix of another and the all-ones padding matches none, so a greedy
# 5-then-6-bit walk decodes a digits-only string exactly.
DIG5 = {"00000": "0", "00001": "1", "00010": "2"}
DIG6 = {"011001": "3", "011010": "4", "011011": "5", "011100": "6",
        "011101": "7", "011110": "8", "011111": "9"}


def frame(ftype, flags, sid, payload):
    return (len(payload).to_bytes(3, "big") + bytes([ftype, flags])
            + sid.to_bytes(4, "big") + payload)


def hpack_get(path, authority):
    return (bytes([0x82])                                  # :method GET
            + bytes([0x86])                                # :scheme http
            + bytes([0x01, len(authority)]) + authority     # :authority
            + bytes([0x04, len(path)]) + path.encode())     # :path


def huff_digits(raw):
    bits = "".join(f"{b:08b}" for b in raw)
    out, i = [], 0
    while i + 5 <= len(bits):
        g5 = bits[i:i + 5]
        if g5 in DIG5:
            out.append(DIG5[g5]); i += 5; continue
        if i + 6 <= len(bits) and bits[i:i + 6] in DIG6:
            out.append(DIG6[bits[i:i + 6]]); i += 6; continue
        break
    return "".join(out)


def first_status(block):
    """`:status` of the first HPACK field (static-indexed or literal)."""
    b = block[0]
    if b & 0x80:                                # indexed static field
        return STATIC_STATUS.get(b & 0x7F)
    if (b & 0xF0) in (0x00, 0x10, 0x40) and (b & 0x0F) == 8:  # name :status
        v = block[1]
        raw = block[2:2 + (v & 0x7F)]
        if v & 0x80:
            return huff_digits(raw) or None
        return raw.decode("ascii", "replace").strip() or None
    return None


def recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


sock = socket.create_connection(("127.0.0.1", PORT), timeout=15)
sock.settimeout(0.2)
auth = ("127.0.0.1:%d" % PORT).encode()
sock.sendall(PREFACE + frame(0x4, 0, 0, b""))            # SETTINGS
sock.sendall(frame(0x1, 0x5, 1, hpack_get("/hang", auth)))  # park stream 1
log = open(OUT, "a")
log.write("PARKED\n")
log.flush()

status, body, sent_probe, pending_ping = {}, {}, False, None
end = time.time() + 14
while time.time() < end:
    if 3 in body and 5 in body:
        break
    try:
        hdr = recv_exact(sock, 9)
    except EOFError:
        break
    except (socket.timeout, OSError):
        continue
    n = int.from_bytes(hdr[0:3], "big")
    ftype, flags = hdr[3], hdr[4]
    sid = int.from_bytes(hdr[5:9], "big") & 0x7FFFFFFF
    payload = recv_exact(sock, n) if n else b""
    if ftype == 0x4 and not flags & 1:                    # SETTINGS
        sock.sendall(frame(0x4, 1, 0, b""))
    elif ftype == 0x7 and not sent_probe:                 # drain GOAWAY #1
        sock.sendall(frame(0x1, 0x5, 3,
                           hpack_get("/openrusty/ready", auth)))
        sock.sendall(frame(0x1, 0x5, 5,
                           hpack_get("/openrusty/live", auth)))
        sent_probe = True
        if pending_ping is not None:
            sock.sendall(frame(0x6, 1, 0, pending_ping))
            pending_ping = None
    elif ftype == 0x6 and not flags & 1:                  # PING
        if sent_probe:
            sock.sendall(frame(0x6, 1, 0, payload))
        else:
            pending_ping = payload
    elif ftype == 0x1 and sid in (3, 5) and sid not in status:
        status[sid] = first_status(payload)
    elif ftype == 0x0 and sid in (3, 5):                  # DATA
        body[sid] = body.get(sid, b"") + payload
log.write("READY %s %s\n" % (status.get(3, "-"),
                             body.get(3, b"").decode("utf-8", "replace").strip()))
log.write("LIVE %s %s\n" % (status.get(5, "-"),
                            body.get(5, b"").decode("utf-8", "replace").strip()))
log.flush()
# Hold the parked connection open past the grace window so the drain's
# bounded wait is really exercised (the process must NOT exit early).
time.sleep(11)
PY
PIDS+=($!)
for _ in $(seq 1 50); do
    grep -q PARKED "$TMP/drain/probe.out" 2>/dev/null && break
    sleep 0.1
done
grep -q PARKED "$TMP/drain/probe.out" \
    || { echo "FATAL: drain probe never parked its hang stream"; exit 1; }
require_alive "drain gateway" "$DRAIN_PID" || exit 1

TERM_T0=$(date +%s%3N)
kill -TERM "$DRAIN_PID"
sleep 1
check "graceful drain: /openrusty/ready answers 503 draining" \
    bash -c "grep -Eq '^READY 503 .*(draining)' '$TMP/drain/probe.out'"
check "graceful drain: /openrusty/live stays 200" \
    bash -c "grep -q '^LIVE 200 ' '$TMP/drain/probe.out'"
DRAIN_RC=0
wait "$DRAIN_PID" || DRAIN_RC=$?
DRAIN_ELAPSED=$(( $(date +%s%3N) - TERM_T0 ))
check "graceful drain: exits 0 after waiting out the grace window" \
    bash -c "test $DRAIN_RC = 0 && test $DRAIN_ELAPSED -ge 6000 \
             && test $DRAIN_ELAPSED -le 11000"
