# integration drill section 60: dynamic wasm API (delegates to
# lib-dynamic-drill.sh) and upstream TLS - CA pinning, wrong-CA rejection,
# insecure skip (sections 31-32).

echo "== 31. dynamic wasm API (POST /api/v1/dynamic/{name}) =="
# Boot config enabled [dynamic] with dir=$TMP/dynamic (echo.wasm +
# reverse.wasm copied at setup) and [dynamic.settings.echo] greeting.
# The drill lives in lib-dynamic-drill.sh (keeps this file in size cap).
dynamic_drill

echo "== 32. upstream TLS (https peers, CA pinning, insecure) =="
# A private CA signs a leaf for localhost/127.0.0.1; a second CA exists
# only to prove pinning rejects it. The https upstream is python's
# http.server wrapped in an ssl context (stdlib-only echo).
mkdir -p "$TMP/tls"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TMP/tls/ca.key" -out "$TMP/tls/ca.pem" \
    -days 2 -subj "/CN=it-ca" >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TMP/tls/other-ca.key" \
    -out "$TMP/tls/other-ca.pem" -days 2 -subj "/CN=other-ca" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$TMP/tls/leaf.key" -out "$TMP/tls/leaf.csr" \
    -subj "/CN=localhost" >/dev/null 2>&1
openssl x509 -req -in "$TMP/tls/leaf.csr" -CA "$TMP/tls/ca.pem" -CAkey "$TMP/tls/ca.key" \
    -CAcreateserial -out "$TMP/tls/leaf.pem" -days 2 \
    -extfile <(printf 'subjectAltName=DNS:localhost,IP:127.0.0.1') >/dev/null 2>&1
cat > "$TMP/tls/serve.py" <<'PY'
import ssl, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"node":"tls-node"}\n'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[2], sys.argv[3])
srv = HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
python3 "$TMP/tls/serve.py" 19301 "$TMP/tls/leaf.pem" "$TMP/tls/leaf.key" \
    > "$TMP/logs/tls-up.log" 2>&1 &
PIDS+=($!)
wait_port 19301 10 || { echo "FATAL: https echo did not come up"; tail -5 "$TMP/logs/tls-up.log"; exit 1; }

# Route /tls through an https upstream pinned to the signing CA.
cat >> "$TMP/openrusty.toml" <<CONF

[[upstreams]]
name = "tls-up"
balancer = "swrr"
retries = 0
connect_timeout_ms = 1000
  [[upstreams.peers]]
  addr = "127.0.0.1:19301"
  [upstreams.tls]
  server_name = "localhost"
  ca_cert = "$TMP/tls/ca.pem"

[[routes]]
path_prefix = "/tls"
upstream = "tls-up"
timeout_ms = 5000
CONF
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
TLS_BODY="$(curl -s --max-time 8 "$GATE/tls/" || true)"
TLS_CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/tls/" || true)"
check "upstream TLS: CA-verified peer forwards (200 + relayed body)" \
    bash -c "test '$TLS_CODE' = 200 && printf '%s' '$TLS_BODY' | grep -q tls-node"

# Wrong CA: rustls refuses the handshake, the gateway answers with its own
# 502 (not anything relayed from a peer).
sed -i "s|ca_cert = \"$TMP/tls/ca.pem\"|ca_cert = \"$TMP/tls/other-ca.pem\"|" "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/tls/" || true)"
check "upstream TLS: wrong CA rejected (502)" test "$CODE" = "502"
BODY="$(curl -s --max-time 8 "$GATE/tls/" || true)"
check "upstream TLS: 502 body is the gateway's own shape" \
    bash -c "printf '%s' '$BODY' | grep -q '^502'"

# insecure_skip_verify: forwards again without any CA anchored.
sed -i "s|ca_cert = \"$TMP/tls/other-ca.pem\"|insecure_skip_verify = true|" "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/tls/" || true)"
check "upstream TLS: insecure_skip_verify forwards (200)" test "$CODE" = "200"
