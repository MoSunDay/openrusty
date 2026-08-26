#!/usr/bin/env bash
# OpenRusty local benchmark matrix (method: docs/perf-nginx-comparison-2026-08-25.md).
#
# Six scenarios x two paths x three concurrencies against local echo upstreams:
#   bare       direct to 127.0.0.1:9001-9003 (baseline)
#   or-warn    openrusty, log_level=warn (access log off), no plugins
#   or-info    openrusty, log_level=info  (access log on),  no plugins
#   or-kvprobe openrusty, log_level=warn, kv-probe.wasm (all 8 phases)
#   nginx-off  nginx in a private instance/pidfile, access_log off
#   nginx-on   same nginx, access_log on
# nginx is fairness-aligned with the gateway's upstream behavior: HTTP/1.1 +
# Connection "" + keepalive 256 + Host $upstream_addr + X-Forwarded-For +
# proxy_buffering off. CPU% comes from /proc/<pid>/stat (utime+stime, CLK_TCK),
# peak RSS from VmHWM. Rows land in build/bench/results-*.jsonl and a markdown
# summary in build/bench/summary-*.md. The system nginx (:80) and the systemd
# openrusty (:8180) are never touched. Override: DURATION WARMUP CONCURRENCY PATHS.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

GATE_PORT=18181
NGX_PORT=18182
ECHO_PORTS="9001 9002 9003"
DURATION="${DURATION:-10}"
WARMUP="${WARMUP:-2}"
CONCURRENCY="${CONCURRENCY:-100 500 2000}"
PATHS="${PATHS:-/ /echo}"
CLK_TCK="$(getconf CLK_TCK)"
WORK="$(mktemp -d /tmp/openrusty-bench.XXXXXX)"
OUT_DIR="$ROOT/build/bench"
STAMP="$(date +%Y%m%d-%H%M%S)"
RESULTS="$OUT_DIR/results-$STAMP.jsonl"
SUMMARY="$OUT_DIR/summary-$STAMP.md"
PIDS_TO_KILL=()
GATE_PID=""

mkdir -p "$OUT_DIR"
ulimit -n 65535 2>/dev/null || echo "WARN: fd limit stays $(ulimit -n)"

cleanup() {
    if [[ -s "$WORK/nginx.pid" ]]; then
        kill "$(cat "$WORK/nginx.pid")" 2>/dev/null || true
    fi
    for p in "${PIDS_TO_KILL[@]:-}"; do kill "$p" 2>/dev/null || true; done
    sleep 0.5
    if [[ -s "$WORK/nginx.pid" ]]; then
        kill -9 "$(cat "$WORK/nginx.pid")" 2>/dev/null || true
    fi
    for p in "${PIDS_TO_KILL[@]:-}"; do kill -9 "$p" 2>/dev/null || true; done
    if [[ "${KEEP_WORK:-0}" == 1 ]]; then say "keep workdir: $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

say() { echo "== $*"; }

wait_http() { # url
    for _ in $(seq 1 100); do
        if curl -s -o /dev/null --max-time 2 "$1"; then return 0; fi
        sleep 0.1
    done
    return 1
}

pids_cpu_ticks() { # pids... -> summed utime+stime ticks
    local total=0 stat t
    for p in "$@"; do
        stat="$(cat "/proc/$p/stat" 2>/dev/null || true)"
        if [[ -n "$stat" ]]; then
            t="$(echo "$stat" | sed 's/^[0-9]* (.*) //' | awk '{print $12 + $13}')"
            total=$((total + t))
        fi
    done
    echo "$total"
}

pids_vmhwm_kb() { # pids... -> summed per-process VmHWM peaks (kB)
    local total=0 v
    for p in "$@"; do
        v="$(awk '/^VmHWM:/{print $2}' "/proc/$p/status" 2>/dev/null || true)"
        total=$((total + ${v:-0}))
    done
    echo "$total"
}

nginx_pids() { # master (pidfile) + live workers, newline separated
    local master
    master="$(cat "$WORK/nginx.pid" 2>/dev/null || true)"
    [[ -n "$master" ]] || return 0
    echo "$master"
    pgrep -P "$master" 2>/dev/null || true
}

write_gate_conf() { # file log_level plugin_dir order
    {   echo '[server]'
        echo "listen = \"127.0.0.1:$GATE_PORT\""
        echo "log_level = \"$2\""
        echo
        echo '[plugins]'
        echo "dir = \"$3\""
        echo "order = [$4]"
        echo 'timeout_ms = 50'
        echo 'max_memory_mb = 16'
        echo 'on_failure = "fail_open"'
        echo
        echo '[[upstreams]]'
        echo 'name = "vllm"'
        echo 'balancer = "swrr"'
        echo 'retries = 2'
        echo 'connect_timeout_ms = 2000'
        for p in $ECHO_PORTS; do
            echo '  [[upstreams.peers]]'
            echo "  addr = \"127.0.0.1:$p\""
            echo '  weight = 1'
        done
        echo '  [upstreams.health]'
        echo '  max_fails = 3'
        echo '  fail_window_s = 10'
        echo '  fail_timeout_s = 10'
        echo
        echo '[[routes]]'
        echo 'path_prefix = "/"'
        echo 'upstream = "vllm"'
        echo 'timeout_ms = 30000'
    } > "$1"
}

start_gate() { # scenario log_level plugin_dir order -> sets GATE_PID
    write_gate_conf "$WORK/gate-$1.toml" "$2" "$3" "$4"
    "$ROOT/target/release/openrusty" "$WORK/gate-$1.toml" > "$WORK/gate-$1.log" 2>&1 &
    GATE_PID=$!
    PIDS_TO_KILL+=("$GATE_PID")
    if ! wait_http "http://127.0.0.1:$GATE_PORT/"; then
        say "gateway $1 did not come up (see $WORK/gate-$1.log)"
        exit 1
    fi
}

write_nginx_conf() { # file access(on|off)
    # "off" takes no format name; a file path takes ours
    local alog="off"
    if [[ "$2" == "on" ]]; then alog="$WORK/nginx-access.log bench"; fi
    cat > "$1" <<CONF
worker_processes auto;
worker_rlimit_nofile 65535;
error_log $WORK/nginx-error.log warn;
pid $WORK/nginx.pid;
events { worker_connections 16384; }
http {
    log_format bench '\$status \$uri \$body_bytes_sent \$request_time';
    access_log $alog;
    client_body_temp_path $WORK/nginx/client_body;
    proxy_temp_path $WORK/nginx/proxy;
    fastcgi_temp_path $WORK/nginx/fastcgi;
    uwsgi_temp_path $WORK/nginx/uwsgi;
    scgi_temp_path $WORK/nginx/scgi;
    upstream bench_up {
        server 127.0.0.1:9001;
        server 127.0.0.1:9002;
        server 127.0.0.1:9003;
        keepalive 256;
    }
    server {
        listen 127.0.0.1:$NGX_PORT;
        location / {
            proxy_pass http://bench_up;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_set_header Host \$upstream_addr;
            proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
            proxy_buffering off;
        }
    }
}
CONF
}

start_nginx() { # on|off
    mkdir -p "$WORK/nginx"
    write_nginx_conf "$WORK/nginx-$1.conf" "$1"
    if ! nginx -t -c "$WORK/nginx-$1.conf" >/dev/null 2>&1; then
        say "nginx config invalid"; exit 1
    fi
    nginx -c "$WORK/nginx-$1.conf"
    for _ in $(seq 1 50); do
        if [[ -s "$WORK/nginx.pid" ]] && curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$NGX_PORT/"; then break; fi
        sleep 0.1
    done
}

stop_nginx() {
    local master workers
    if [[ -s "$WORK/nginx.pid" ]]; then
        master="$(cat "$WORK/nginx.pid")"
        workers="$(pgrep -P "$master" 2>/dev/null || true)"
        kill "$master" 2>/dev/null || true
        sleep 0.5
        # workers can outlive the master while draining; take them down too
        for w in $workers; do kill "$w" 2>/dev/null || true; done
        sleep 0.5
        kill -9 $workers 2>/dev/null || true
    fi
}

run_case() { # scenario path c srv_pids_str target...
    local scenario="$1" path="$2" c="$3" srv="$4"; shift 4
    local t0 t1 hwm json
    t0="$(pids_cpu_ticks $srv)"
    json="$(python3 scripts/bench_loadgen.py "$@" \
        --path "$path" --concurrency "$c" --duration "$DURATION")"
    t1="$(pids_cpu_ticks $srv)"
    hwm="$(pids_vmhwm_kb $srv)"
    ROW_JSON="$json" SCENARIO="$scenario" PATH_="$path" C="$c" T0="$t0" T1="$t1" \
    HWM="$hwm" CLK="$CLK_TCK" RESULTS="$RESULTS" python3 <<'PY'
import json, os
row = json.loads(os.environ["ROW_JSON"])
cpu_s = max(0, int(os.environ["T1"]) - int(os.environ["T0"])) / int(os.environ["CLK"])
row.update(scenario=os.environ["SCENARIO"], path=os.environ["PATH_"], c=int(os.environ["C"]),
           cpu_pct=round(cpu_s / row["wall_s"] * 100, 1),
           cpu_us_per_req=round(cpu_s * 1e6 / row["count"], 1) if row["count"] else 0.0,
           vmhwm_mb=round(int(os.environ["HWM"]) / 1024, 1))
with open(os.environ["RESULTS"], "a") as fh:
    fh.write(json.dumps(row, ensure_ascii=False) + "\n")
print(f'{row["scenario"]:<10} {row["path"]:<6} c={row["c"]:<5} rps={row["rps"]:<9.1f}'
      f'p50={row["p50_ms"]:<7.2f} p90={row["p90_ms"]:<7.2f} p99={row["p99_ms"]:<8.2f}'
      f'err={row["err"]:<4} cpu%={row["cpu_pct"]:<6.1f} us/req={row["cpu_us_per_req"]:<7.1f}'
      f'hwm={row["vmhwm_mb"]}MB')
PY
    sleep 1
}

say "env check"
command -v nginx >/dev/null || { say "nginx not installed"; exit 1; }
[[ -x target/release/openrusty ]] || { say "run: cargo build --release -p openrusty-server"; exit 1; }
ECHO_PIDS=""
for p in $ECHO_PORTS; do
    if ! curl -s -o /dev/null --max-time 2 "http://127.0.0.1:$p/"; then
        say "echo upstream :$p down"; exit 1
    fi
    ECHO_PIDS+="$(systemctl show "openrusty-echo@$p" -p MainPID --value) "
done
if [[ -f build/plugins/kv-probe.wasm ]]; then :; else
    bash scripts/build-plugins.sh >/dev/null 2>&1
fi
[[ -f build/plugins/kv-probe.wasm ]] || { say "kv-probe.wasm missing"; exit 1; }
if ss -ltn 2>/dev/null | grep -qE ":(18181|18182) "; then say "bench ports busy"; exit 1; fi
mkdir -p "$WORK/plugins-warn" "$WORK/plugins-info" "$WORK/plugins-kvprobe"
cp build/plugins/kv-probe.wasm "$WORK/plugins-kvprobe/"

BARE_ARGS=()
BARE_SRV=""
for p in $ECHO_PORTS; do
    BARE_ARGS+=(--target "127.0.0.1:$p")
    BARE_SRV+="$(systemctl show "openrusty-echo@$p" -p MainPID --value) "
done
printf 'commit %s | %s | nproc %s | %s | CLK_TCK %s | fd %s | duration %ss\n' \
    "$(git rev-parse --short HEAD)" "$(date -Is)" "$(nproc)" \
    "$(nginx -v 2>&1)" "$CLK_TCK" "$(ulimit -n)" "$DURATION" > "$SUMMARY"

say "matrix: 6 scenarios x ${PATHS} x c=${CONCURRENCY} (duration ${DURATION}s)"
for SCENARIO in bare or-warn or-info or-kvprobe nginx-off nginx-on; do
    case "$SCENARIO" in
        bare)       SRV="$BARE_SRV"; TARGETS=("${BARE_ARGS[@]}") ;;
        or-warn)    say "openrusty or-warn up"
                    start_gate warn warn "$WORK/plugins-warn" ''
                    SRV="$GATE_PID"; TARGETS=(--target "127.0.0.1:$GATE_PORT") ;;
        or-info)    say "openrusty or-info up"
                    start_gate info info "$WORK/plugins-info" ''
                    SRV="$GATE_PID"; TARGETS=(--target "127.0.0.1:$GATE_PORT") ;;
        or-kvprobe) say "openrusty or-kvprobe up"
                    start_gate kvprobe warn "$WORK/plugins-kvprobe" '"kv-probe"'
                    SRV="$GATE_PID"; TARGETS=(--target "127.0.0.1:$GATE_PORT") ;;
        nginx-off)  say "nginx (access_log off) up"; start_nginx off
                    SRV="$(nginx_pids | tr '\n' ' ')"; TARGETS=(--target "127.0.0.1:$NGX_PORT") ;;
        nginx-on)   say "nginx (access_log on) up"; start_nginx on
                    SRV="$(nginx_pids | tr '\n' ' ')"; TARGETS=(--target "127.0.0.1:$NGX_PORT") ;;
    esac
    for PATH_ in $PATHS; do
        # warmup shakes out lazy init (wasm instantiation, pools, accept queues)
        python3 scripts/bench_loadgen.py "${TARGETS[@]}" \
            --path "$PATH_" --concurrency 100 --duration "$WARMUP" >/dev/null
        for C in $CONCURRENCY; do
            run_case "$SCENARIO" "$PATH_" "$C" "$SRV" "${TARGETS[@]}"
        done
    done
    case "$SCENARIO" in
        or-*) for p in "${PIDS_TO_KILL[@]:-}"; do kill "$p" 2>/dev/null || true; done
              PIDS_TO_KILL=(); sleep 0.5 ;;
        nginx-*) stop_nginx ;;
    esac
done

say "summary"
RESULTS="$RESULTS" SUMMARY="$SUMMARY" python3 <<'PY'
import json, os
rows = [json.loads(l) for l in open(os.environ["RESULTS"]) if l.strip()]
with open(os.environ["SUMMARY"], "a") as out:
    out.write("\n## full matrix\n\n")
    out.write("| scenario | path | c | rps | p50 | p90 | p99 | err | cpu% | us/req | hwm MB |\n")
    out.write("|" + "---|" * 11 + "\n")
    for r in rows:
        out.write(f"| {r['scenario']} | {r['path']} | {r['c']} | {r['rps']} | "
                  f"{r['p50_ms']} | {r['p90_ms']} | {r['p99_ms']} | {r['err']} | "
                  f"{r['cpu_pct']} | {r['cpu_us_per_req']} | {r['vmhwm_mb']} |\n")
    errs = [r for r in rows if r["err"]]
    if errs:
        out.write(f"\nERRORS: {len(errs)} rows with err>0: "
                  + ", ".join(f"{r['scenario']}/{r['path']}/c{r['c']}" for r in errs) + "\n")
    def rps_of(scen, path, c):
        return next((r["rps"] for r in rows if r["scenario"] == scen and r["path"] == path and r["c"] == c), 0.0)
    ok = all(rps_of("bare", r["path"], r["c"]) > r["rps"]
             for r in rows if r["scenario"] != "bare")
    out.write(f"\nsanity bare > proxied rps (all cells): "
              f"{'PASS' if ok else 'WARN (loadgen single-process ceiling, see report)'}\n")
print(open(os.environ["SUMMARY"]).read(), end="")
PY
say "rows:   $RESULTS"
say "summary: $SUMMARY"
