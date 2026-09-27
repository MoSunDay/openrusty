# integration drill section 96: config surface - a dedicated gateway
# exercising four config-only behaviours end to end: (1) weighted swrr
# gives an exact weight-proportional split over full weight-sum cycles,
# (2) static routes combine `host` + `exact` (host class first, exact
# never prefix-matches), (3) upstream mTLS via tls.client_cert/client_key
# (a peer-less client cert against the same server must fail with 502),
# (4) `server.http1_only = true` keeps plain HTTP/1.1 and refuses the
# h2c prior-knowledge preface. Own ports + own TOML under $TMP.
CFG_PORT="${CFG_PORT:-18220}"
CFG_SWRR_HEAVY_PORT="${CFG_SWRR_HEAVY_PORT:-18221}"
CFG_SWRR_LIGHT_PORT="${CFG_SWRR_LIGHT_PORT:-18222}"
CFG_HOST_A_PORT="${CFG_HOST_A_PORT:-18223}"
CFG_HOST_B_PORT="${CFG_HOST_B_PORT:-18224}"
CFG_MTLS_PORT="${CFG_MTLS_PORT:-18225}"

echo "== 96. config surface: weighted swrr, host+exact routes, upstream mTLS, http1_only =="
mkdir -p "$TMP/cfgsurf/plugins" "$TMP/cfgsurf/tls"

# Four echo upstreams with distinct node names.
for spec in "$CFG_SWRR_HEAVY_PORT:swrr-heavy" "$CFG_SWRR_LIGHT_PORT:swrr-light" \
            "$CFG_HOST_A_PORT:hit-a" "$CFG_HOST_B_PORT:hit-b"; do
    port="${spec%%:*}"; node="${spec#*:}"
    "$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:$port" "$node" \
        > "$TMP/cfgsurf/echo-$node.log" 2>&1 &
    PIDS+=($!)
done
wait_port $CFG_SWRR_HEAVY_PORT 10 && wait_port $CFG_SWRR_LIGHT_PORT 10 \
    && wait_port $CFG_HOST_A_PORT 10 && wait_port $CFG_HOST_B_PORT 10 \
    || { echo "FATAL: config-surface echo upstreams did not come up"; exit 1; }

# Own CA + server leaf (mTLS peer) + client cert, §32 openssl pattern.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$TMP/cfgsurf/tls/ca.key" \
    -out "$TMP/cfgsurf/tls/ca.pem" -days 2 -subj "/CN=cfgsurf-ca" >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$TMP/cfgsurf/tls/leaf.key" \
    -out "$TMP/cfgsurf/tls/leaf.csr" -subj "/CN=localhost" >/dev/null 2>&1
openssl x509 -req -in "$TMP/cfgsurf/tls/leaf.csr" -CA "$TMP/cfgsurf/tls/ca.pem" \
    -CAkey "$TMP/cfgsurf/tls/ca.key" -CAcreateserial -out "$TMP/cfgsurf/tls/leaf.pem" \
    -days 2 -extfile <(printf 'subjectAltName=DNS:localhost,IP:127.0.0.1') >/dev/null 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$TMP/cfgsurf/tls/client.key" \
    -out "$TMP/cfgsurf/tls/client.csr" -subj "/CN=cfgsurf-client" >/dev/null 2>&1
openssl x509 -req -in "$TMP/cfgsurf/tls/client.csr" -CA "$TMP/cfgsurf/tls/ca.pem" \
    -CAkey "$TMP/cfgsurf/tls/ca.key" -CAcreateserial -out "$TMP/cfgsurf/tls/client.pem" \
    -days 2 >/dev/null 2>&1

# https upstream that REQUIRES a client certificate (threaded so a failed
# handshake from the cert-less upstream never wedges the accept loop).
cat > "$TMP/cfgsurf/tls/mtls.py" <<'PY'
import json
import ssl
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"node": "mtls-node"}).encode() + b"\n"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
ctx.load_cert_chain(sys.argv[2], sys.argv[3])
ctx.load_verify_locations(sys.argv[4])
ctx.verify_mode = ssl.CERT_REQUIRED
srv = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler)
srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
srv.serve_forever()
PY
python3 "$TMP/cfgsurf/tls/mtls.py" "$CFG_MTLS_PORT" "$TMP/cfgsurf/tls/leaf.pem" \
    "$TMP/cfgsurf/tls/leaf.key" "$TMP/cfgsurf/tls/ca.pem" \
    > "$TMP/cfgsurf/mtls-up.log" 2>&1 &
PIDS+=($!)
wait_port $CFG_MTLS_PORT 10 \
    || { echo "FATAL: mTLS https upstream did not come up"; exit 1; }

cat > "$TMP/cfgsurf/gw.toml" <<CONF
[server]
listen = "127.0.0.1:$CFG_PORT"
log_level = "info"
http1_only = true

[plugins]
dir = "$TMP/cfgsurf/plugins"

[[upstreams]]
name = "swrr-uw"
balancer = "swrr"
retries = 0
  [[upstreams.peers]]
  addr = "127.0.0.1:$CFG_SWRR_HEAVY_PORT"
  weight = 3
  [[upstreams.peers]]
  addr = "127.0.0.1:$CFG_SWRR_LIGHT_PORT"
  weight = 1

[[upstreams]]
name = "host-a-uw"
  [[upstreams.peers]]
  addr = "127.0.0.1:$CFG_HOST_A_PORT"

[[upstreams]]
name = "host-b-uw"
  [[upstreams.peers]]
  addr = "127.0.0.1:$CFG_HOST_B_PORT"

[[upstreams]]
name = "mtls-ok"
  [[upstreams.peers]]
  addr = "127.0.0.1:$CFG_MTLS_PORT"
  [upstreams.tls]
  server_name = "localhost"
  ca_cert = "$TMP/cfgsurf/tls/ca.pem"
  client_cert = "$TMP/cfgsurf/tls/client.pem"
  client_key = "$TMP/cfgsurf/tls/client.key"

[[upstreams]]
name = "mtls-nocert"
  [[upstreams.peers]]
  addr = "127.0.0.1:$CFG_MTLS_PORT"
  [upstreams.tls]
  server_name = "localhost"
  ca_cert = "$TMP/cfgsurf/tls/ca.pem"

[[routes]]
path_prefix = "/swrr"
upstream = "swrr-uw"
timeout_ms = 5000

[[routes]]
path_prefix = "/hit"
exact = true
host = "api.example.com"
upstream = "host-a-uw"
timeout_ms = 5000

[[routes]]
path_prefix = "/mtls"
upstream = "mtls-ok"
timeout_ms = 5000

[[routes]]
path_prefix = "/mtls-nocert"
upstream = "mtls-nocert"
timeout_ms = 5000

[[routes]]
path_prefix = "/"
upstream = "host-b-uw"
timeout_ms = 5000
CONF
"$ROOT/target/debug/openrusty" "$TMP/cfgsurf/gw.toml" > "$TMP/cfgsurf/gw.log" 2>&1 &
CFG_PID=$!
PIDS+=($CFG_PID)
wait_port $CFG_PORT 10 || { echo "FATAL: config-surface gateway did not come up"; exit 1; }
CFG_GATE="http://127.0.0.1:$CFG_PORT"

echo "  -- weighted swrr (3:1 over 40 requests = 10 full cycles)"
SWRR_HEAVY=0; SWRR_LIGHT=0
for _ in $(seq 1 40); do
    node="$(curl -s --max-time 5 "$CFG_GATE/swrr" | \
        python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null || true)"
    case "$node" in
        swrr-heavy) SWRR_HEAVY=$((SWRR_HEAVY + 1)) ;;
        swrr-light) SWRR_LIGHT=$((SWRR_LIGHT + 1)) ;;
    esac
done
check "swrr weights: exact 30/10 split over full cycles" \
    bash -c "test $SWRR_HEAVY -eq 30 && test $SWRR_LIGHT -eq 10"

echo "  -- host + exact route selection"
node_for() { # host path -> upstream node name
    curl -s --max-time 5 -H "Host: $1" "$CFG_GATE$2" | \
        python3 -c 'import sys,json;print(json.load(sys.stdin)["node"])' 2>/dev/null || true
}
check "route host+exact: matching Host and path hits upstream A" \
    test "$(node_for api.example.com /hit)" = "hit-a"
check "route host+exact: same path, other Host falls to catch-all B" \
    test "$(node_for other.example.com /hit)" = "hit-b"
check "route host+exact: exact never prefix-matches (/hit/extra -> B)" \
    test "$(node_for api.example.com /hit/extra)" = "hit-b"

echo "  -- upstream mTLS"
MTLS_CODE="$(curl -s -o "$TMP/cfgsurf/mtls-ok.out" -w '%{http_code}' \
    --max-time 8 "$CFG_GATE/mtls/" || true)"
check "upstream mTLS: client cert presented, peer forwards (200)" \
    bash -c "test '$MTLS_CODE' = 200 && grep -q mtls-node '$TMP/cfgsurf/mtls-ok.out'"
MTLS_NOCERT_CODE="$(curl -s -o /dev/null -w '%{http_code}' \
    --max-time 8 "$CFG_GATE/mtls-nocert/" || true)"
check "upstream mTLS: no client cert -> handshake fails (502)" \
    test "$MTLS_NOCERT_CODE" = "502"

echo "  -- http1_only inbound"
H1_VERSION="$(curl -s -o /dev/null -w '%{http_version}' --max-time 5 "$CFG_GATE/" || true)"
check "http1_only: plain HTTP/1.1 answers 1.1" test "$H1_VERSION" = "1.1"
H2C_RC=0
H2C_VERSION="$(curl -s -o /dev/null -w '%{http_version}' \
    --http2-prior-knowledge --max-time 5 "$CFG_GATE/" 2>/dev/null || true)" \
    || H2C_RC=$?
check "http1_only: h2c prior-knowledge preface is refused (no h2)" \
    bash -c "test $H2C_RC -ne 0 || test '$H2C_VERSION' != '2'"
