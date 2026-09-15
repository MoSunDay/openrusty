# integration drill section 00: plugin + cargo builds, drill config
# setup, config dry run (openrusty -t), gateway + echo-upstream startup.
# Sourced by scripts/integration.sh into its shell (shares check/TMP/PIDS globals).
echo "== build =="
if ! bash scripts/build-plugins.sh; then
    echo "FATAL: plugin build failed" >&2
    exit 1
fi
if ! out="$(cargo build -p openrusty-server --example echo_upstream 2>&1)"; then
    echo "FATAL: echo_upstream example build failed" >&2
    printf '%s\n' "$out" | tail -20 >&2
    exit 1
fi
if ! out="$(cargo build -p openrusty-server 2>&1)"; then
    echo "FATAL: gateway build failed" >&2
    printf '%s\n' "$out" | tail -20 >&2
    exit 1
fi
for artifact in "$ROOT/target/debug/openrusty" \
                "$ROOT/target/debug/examples/echo_upstream" \
                "$ROOT/build/plugins/vllm-kv-scheduler.wasm" \
                "$ROOT/build/plugins/kv-probe.wasm" \
                "$ROOT/build/plugins/dynamic-echo.wasm" \
                "$ROOT/build/plugins/dynamic-reverse.wasm"; do
    if [ ! -e "$artifact" ]; then
        echo "FATAL: missing build artifact: $artifact" >&2
        exit 1
    fi
done

echo "== setup =="
mkdir -p "$TMP/plugins" "$TMP/dynamic" "$TMP/logs"
cp build/plugins/vllm-kv-scheduler.wasm "$TMP/plugins/"
# Dynamic-API modules live in their own dir (never build/plugins: the
# endpoint must not serve the pipeline plugins).
cp build/plugins/dynamic-echo.wasm "$TMP/dynamic/echo.wasm"
cp build/plugins/dynamic-reverse.wasm "$TMP/dynamic/reverse.wasm"

cat > "$TMP/openrusty.toml" <<CONF
[server]
listen = "127.0.0.1:$GATE_PORT"
log_level = "info"

[plugins]
dir = "$TMP/plugins"
order = ["vllm-kv-scheduler"]
timeout_ms = 250
max_memory_mb = 16
on_failure = "fail_open"

[plugins.settings.vllm-kv-scheduler]
extract = "query:task"
affinity_ttl_s = "6"
max_tasks_per_node = "0"

# Dynamic single-module execution API (POST /api/v1/dynamic/<name>).
# Routes are mounted at boot because the config is final here; the
# timeout is raised above the default 50ms to stay safe under the
# concurrency check below.
[dynamic]
dir = "$TMP/dynamic"
timeout_ms = 100

[dynamic.settings.echo]
greeting = "hi"

[[upstreams]]
name = "vllm"
balancer = "swrr"
retries = 2
connect_timeout_ms = 1000
  [[upstreams.peers]]
  addr = "127.0.0.1:19101"
  [[upstreams.peers]]
  addr = "127.0.0.1:19102"
  [[upstreams.peers]]
  addr = "127.0.0.1:19103"
  [upstreams.health]
  max_fails = 2
  fail_window_s = 5
  fail_timeout_s = 2
  [upstreams.health.active]
  interval_ms = 500
  timeout_ms = 500
  path = "/"
  unhealthy_threshold = 2
  healthy_threshold = 2

[[routes]]
path_prefix = "/"
upstream = "vllm"
timeout_ms = 10000
CONF

echo "== config dry run (openrusty -t) =="
# nginx-style dry run before anything is started: the drill config passes,
# malformed TOML fails, and a corrupt .wasm in the plugins dir fails.
dryrun_ok() { "$ROOT/target/debug/openrusty" -t "$1" >/dev/null 2>&1; }
dryrun_fail() { ! "$ROOT/target/debug/openrusty" -t "$1" >/dev/null 2>&1; }
check "config test: -t accepts valid config" dryrun_ok "$TMP/openrusty.toml"
printf 'not = [valid\n' > "$TMP/bad.toml"
check "config test: -t rejects malformed TOML" dryrun_fail "$TMP/bad.toml"
mkdir -p "$TMP/plugins-broken"
cp "$TMP/plugins/vllm-kv-scheduler.wasm" "$TMP/plugins-broken/"
printf 'not a wasm module' > "$TMP/plugins-broken/broken.wasm"
sed "s#^dir = \"$TMP/plugins\"\$#dir = \"$TMP/plugins-broken\"#" "$TMP/openrusty.toml" \
    > "$TMP/broken-plugin.toml"
check "config test: -t rejects corrupt plugin wasm" dryrun_fail "$TMP/broken-plugin.toml"

for i in 1 2 3; do
    "$ROOT/target/debug/examples/echo_upstream" "127.0.0.1:1910$i" "node$i" \
        > "$TMP/logs/up$i.log" 2>&1 &
    PIDS+=($!)
done
"$ROOT/target/debug/openrusty" "$TMP/openrusty.toml" > "$TMP/logs/gate.log" 2>&1 &
GATE_PID=$!; PIDS+=($GATE_PID)
wait_port 19101 10 && wait_port 19102 10 && wait_port 19103 10 && wait_port $GATE_PORT 10 \
    || { echo "FATAL: processes did not come up"; tail -5 "$TMP/logs"/*.log; exit 1; }
# A process can bind its port and die right after; fail fast on that.
if ! require_alive "startup" "${PIDS[@]}"; then
    tail -5 "$TMP/logs"/*.log
    exit 1
fi
