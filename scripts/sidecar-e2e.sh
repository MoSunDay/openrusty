#!/usr/bin/env bash
# sidecar-e2e: cluster-level sidecar transparent-interception drill
# (S1-S6) against a local single-node k3s. Sibling of
# scripts/cluster-e2e.sh (the ingress-gateway form); this one gates the
# transparent sidecar plane end to end (red lines per docs/sidecar.md):
#   S1 preflight  - cluster CLI (kubectl, else k3s+k3s.yaml), apiserver
#                   /readyz, openrusty binary, docker, helm (chart gate),
#                   docker image import into the k3s containerd, and an
#                   idempotent namespace reset (delete + create).
#   S2 inject     - three workloads: ngx-a (nginx + injected sidecar),
#                   curl-b (curl sleeper + injected sidecar; the exec
#                   client AND the outbound subject), ngx-c (plain
#                   nginx). All Available; A/B init containers exit 0.
#                   A/B carry proxy-log-level=info so the transparent
#                   INFO lines are observable in S3/S5.
#   S3 inbound    - curl-b -> svc-a rides A's inbound interception:
#                    200s, X-Forwarded-For value (B pod IP, the trailing
#                    quoted access-log field) in A's nginx log, the
#                    openrusty_transparent_conns_total{role="inbound",
#                    outcome="http"} counter >= 1, and a sidecar log
#                    dst=podIP:80 (the ORIGINAL destination, not :4143).
#   S4 outbound   - curl-b -> svc-c rides B's outbound interception:
#                    200, {role="outbound",outcome="egress_direct"} >= 1
#                    (egress mode default "direct").
#   S5 lifecycle  - delete A's pod: sidecar exit code 0 (skip-tolerant
#                    if the object vanishes first), shutdown log
#                    "phase 1/3" + "complete", replacement pod Ready
#                    with a clean init, then an observation window of
#                    ready/live/svc-a probes all staying 200.
#   S6 chart demo - helm renders the chart demo (sidecar image = REF):
#                    demo deployment name extracted from `helm template`
#                    (never hardcoded), inbound via pod IP :80 -> 200
#                    through the demo sidecar, demo init exit 0.
# Summary "== CHECKS: N total ==" / "== RESULT: X passed, Y failed ==";
# exit nonzero on any FAIL. Re-runnable (namespace reset in S1).
# --keep preserves the namespace for manual inspection.
# Usage: scripts/sidecar-e2e.sh [--namespace NS] [--image REF]
#   [--window-seconds N] [--keep] [--skip-import] [--skip-chart]
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

BIN="$ROOT/target/release/openrusty"
NS="openrusty-sidecar-e2e" IMAGE_REF="openrusty:local" WINDOW_SECONDS=90
KEEP=0 SKIP_IMPORT=0 SKIP_CHART=0
PASS=0 FAIL=0 LOG_FOLLOW_PID="" HELM_INSTALLED=0
TMP="$(mktemp -d /tmp/openrusty-sidecar-e2e.XXXXXX)"

usage() {
    cat <<'EOF'
usage: scripts/sidecar-e2e.sh [flags]
  --namespace NS      namespace for the drill (default openrusty-sidecar-e2e)
  --image REF         sidecar image ref (default openrusty:local); imported
                      into the k3s containerd unless --skip-import
  --window-seconds N  S5 observation window in seconds (default 90,
                      probed in 5s rounds)
  --keep              keep the namespace + workloads on exit
  --skip-import       assume REF is already present in the containerd
  --skip-chart        skip the S6 chart demo gate
  -h | --help         this text
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
    --namespace) NS="$2"; shift ;;
    --image) IMAGE_REF="$2"; shift ;;
    --window-seconds) WINDOW_SECONDS="$2"; shift ;;
    --keep) KEEP=1 ;;
    --skip-import) SKIP_IMPORT=1 ;;
    --skip-chart) SKIP_CHART=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "sidecar-e2e: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

# ---- helpers (small, composable; no state beyond the drill globals) ----

# helm has no KCTL-style wrapper: give the whole suite the same kubeconfig
# default so raw `helm` calls reach the same cluster kubectl does.
if [ -z "${KUBECONFIG:-}" ] && [ -f /etc/rancher/k3s/k3s.yaml ]; then
    export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
fi

KCTL() { # kubectl when in PATH, else k3s kubectl + the k3s kubeconfig
    if command -v kubectl >/dev/null 2>&1; then kubectl "$@"
    else KUBECONFIG="${KUBECONFIG:-/etc/rancher/k3s/k3s.yaml}" k3s kubectl "$@"; fi
}
have() { command -v "$1" >/dev/null 2>&1; }
have_cluster_cli() { have kubectl || have k3s; }

ctr_import_stdin() { # docker-save tarball on stdin -> k3s containerd
    if have k3s; then k3s ctr images import - >/dev/null 2>&1
    else ctr -a /run/k3s/containerd/containerd.sock images import - >/dev/null 2>&1; fi
}
ctr_images_ls() {
    if have k3s; then k3s ctr images ls
    else ctr -a /run/k3s/containerd/containerd.sock images ls; fi
}
import_image() { docker save "$IMAGE_REF" 2>/dev/null | ctr_import_stdin; }
image_listed() { ctr_images_ls 2>/dev/null | grep -F "$IMAGE_REF" >/dev/null; }
readyz_ok() { KCTL get --raw=/readyz --request-timeout=10s 2>/dev/null | grep -q ok; }
ns_reset() { # idempotent re-runs: drop any prior drill namespace, recreate
    KCTL delete namespace "$NS" --ignore-not-found --timeout=120s >/dev/null 2>&1 || true
    KCTL create namespace "$NS" >/dev/null 2>&1
}

cleanup() { # best effort only; never masks an assertion result
    if [ -n "$LOG_FOLLOW_PID" ]; then kill "$LOG_FOLLOW_PID" 2>/dev/null || true; fi
    if [ "$KEEP" != 1 ] && have_cluster_cli; then
        if [ "$HELM_INSTALLED" = 1 ]; then
            helm uninstall openrusty-demo -n "$NS" --ignore-not-found >/dev/null 2>&1 || true
        fi
        KCTL delete namespace "$NS" --ignore-not-found --timeout=60s >/dev/null 2>&1 || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

check() { # name condition...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then PASS=$((PASS + 1)); echo "PASS: $name"
    else FAIL=$((FAIL + 1)); echo "FAIL: $name"; fi
}

wait_avail() { # deployment-name -> Available within 180s?
    KCTL -n "$NS" wait --for=condition=Available "deployment/$1" --timeout=180s >/dev/null 2>&1
}
pod_of() { # app-label value -> first pod name (empty on none)
    KCTL -n "$NS" get pod -l "app=$1" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null
}
pod_ip() { # app-label value -> first pod IP
    KCTL -n "$NS" get pod -l "app=$1" -o jsonpath='{.items[0].status.podIP}' 2>/dev/null
}
pod_init_exit0() { # pod-name -> initContainerStatuses[0] terminated 0?
    [ -n "$1" ] || return 1
    [ "$(KCTL -n "$NS" get pod "$1" \
        -o jsonpath='{.status.initContainerStatuses[0].state.terminated.exitCode}' 2>/dev/null)" = "0" ]
}
code200() { # deploy container url -> HTTP 200 from inside the cluster?
    [ "$(KCTL -n "$NS" exec "deploy/$1" -c "$2" -- \
        curl -s -o /dev/null --max-time 10 -w '%{http_code}' "$3" 2>/dev/null)" = "200" ]
}
metric_ge1() { # fetch_cmd=(wget|curl) deploy container fixed-pattern
    local fetch="$1" deploy="$2" container="$3" pattern="$4" args=()
    if [ "$fetch" = wget ]; then args=(wget -qO-); else args=(curl -s --max-time 10); fi
    KCTL -n "$NS" exec "deploy/$deploy" -c "$container" -- "${args[@]}" \
        http://127.0.0.1:4191/openrusty/metrics 2>/dev/null \
        | grep -F "$pattern" | awk 'END { exit (NR >= 1 && $NF + 0 >= 1) ? 0 : 1 }'
}

# ---- S1. preflight ----
echo "== S1. preflight =="
check "S1 cluster CLI present (kubectl or k3s)" have_cluster_cli
check "S1 apiserver /readyz ok" readyz_ok
check "S1 openrusty release binary present" test -x "$BIN"
check "S1 docker present" have docker
if [ "$SKIP_CHART" != 1 ]; then check "S1 helm present" have helm; fi
if [ "$SKIP_IMPORT" != 1 ]; then
    check "S1 image imported into k3s containerd ($IMAGE_REF)" import_image
    check "S1 image listed in k3s containerd" image_listed
fi
check "S1 drill namespace recreated" ns_reset

# ---- S2. deploy + inject ----
echo; echo "== S2. deploy + inject =="
write_nginx_dep() { # name [extra pod-template annotation] -> $TMP/<name>.base.yaml
    # (nginx, label app=<name>; the optional 2nd arg adds one annotation,
    #  used to pin the app port on intercepted workloads like ngx-a)
    local extra="${2:+, $2}"
    cat > "$TMP/$1.base.yaml" <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $1
  namespace: $NS
  labels: {app: $1}
spec:
  replicas: 1
  selector: {matchLabels: {app: $1}}
  template:
    metadata:
      labels: {app: $1}
      annotations: {config.openrusty.io/inject: enabled, config.openrusty.io/proxy-log-level: info$extra}
    spec:
      containers:
        - name: nginx
          image: nginx:alpine
          ports: [{name: http, containerPort: 80}]
EOF
}
write_svc() { # svc-name app-label -> $TMP/<svc-name>.svc.yaml
    cat > "$TMP/$1.svc.yaml" <<EOF
apiVersion: v1
kind: Service
metadata:
  name: $1
  namespace: $NS
spec:
  selector: {app: $2}
  ports: [{name: http, port: 80, targetPort: 80}]
EOF
}
inject_apply() { # deployment-name: base -> openrusty inject -> apply both docs
    "$BIN" inject --image "$IMAGE_REF" < "$TMP/$1.base.yaml" > "$TMP/$1.injected.yaml" \
        2> "$TMP/$1.inject.err"
    KCTL apply -f "$TMP/$1.injected.yaml" >/dev/null
}

write_nginx_dep ngx-a 'config.openrusty.io/app-port: "80"'
write_nginx_dep ngx-c
write_svc svc-a ngx-a
write_svc svc-c ngx-c
cat > "$TMP/curl-b.base.yaml" <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: curl-b
  namespace: $NS
  labels: {app: curl-b}
spec:
  replicas: 1
  selector: {matchLabels: {app: curl-b}}
  template:
    metadata:
      labels: {app: curl-b}
      annotations: {config.openrusty.io/inject: enabled, config.openrusty.io/proxy-log-level: info}
    spec:
      containers:
        - name: curl-b
          image: curlimages/curl:8.11.1
          command: ["sh", "-c", "sleep 3600"]
EOF

inject_apply ngx-a
inject_apply curl-b
KCTL apply -f "$TMP/ngx-c.base.yaml" >/dev/null
KCTL -n "$NS" apply -f "$TMP/svc-a.svc.yaml" >/dev/null
KCTL -n "$NS" apply -f "$TMP/svc-c.svc.yaml" >/dev/null

check "S2 deployment ngx-a (injected) Available" wait_avail ngx-a
check "S2 deployment curl-b (injected) Available" wait_avail curl-b
check "S2 deployment ngx-c (plain) Available" wait_avail ngx-c
check "S2 ngx-a init container openrusty-init exit 0" pod_init_exit0 "$(pod_of ngx-a)"
check "S2 curl-b init container openrusty-init exit 0" pod_init_exit0 "$(pod_of curl-b)"

# ---- S3. inbound interception (curl-b -> svc-a -> A sidecar -> nginx) ----
echo; echo "== S3. inbound interception =="
for _ in 1 2 3; do # warm the metrics + access log before asserting
    KCTL -n "$NS" exec deploy/curl-b -c curl-b -- \
        curl -s -o /dev/null --max-time 10 http://svc-a/ >/dev/null 2>&1 || true
done
check "S3 GET http://svc-a/ via inbound interception -> 200" \
    code200 curl-b curl-b http://svc-a/
xff_logged() { # A's nginx 'main' log ends with the XFF value: "<B pod IP>"
    local bip
    bip="$(pod_ip curl-b)"
    [ -n "$bip" ] || return 1
    KCTL -n "$NS" logs deploy/ngx-a -c nginx 2>/dev/null | grep "\"$bip\"\$" >/dev/null
}
check "S3 nginx access log carries X-Forwarded-For (B pod IP)" xff_logged
check "S3 sidecar metric inbound http conns >= 1" metric_ge1 wget ngx-a nginx \
    'openrusty_transparent_conns_total{role="inbound",outcome="http"}'
inbound_orig_dst_ok() { # intercepted line keeps the ORIGINAL dst (podIP:80)
    local p
    p="$(pod_of ngx-a)"
    [ -n "$p" ] || return 1
    KCTL -n "$NS" logs "$p" -c openrusty-proxy 2>/dev/null \
        | sed $'s/\x1b\[[0-9;]*m//g' \
        | grep -F "transparent HTTP intercepted" | grep -F 'role="inbound"' \
        | grep -o "dst=[^ ]*" | grep -v ":4143\$" | grep ":80\$" >/dev/null
}
check "S3 sidecar log: inbound dst=<podIP>:80 (orig dst, not :4143)" inbound_orig_dst_ok

# ---- S4. outbound interception (curl-b -> svc-c, B sidecar egress) ----
echo; echo "== S4. outbound interception =="
check "S4 GET http://svc-c/ via outbound interception -> 200" \
    code200 curl-b curl-b http://svc-c/
check "S4 sidecar metric outbound egress_direct conns >= 1" metric_ge1 curl curl-b curl-b \
    'openrusty_transparent_conns_total{role="outbound",outcome="egress_direct"}'

# ---- S5. lifecycle: pod delete, drain, replacement, observation window ----
echo; echo "== S5. lifecycle =="
P="$(pod_of ngx-a)"
check "S5 target pod resolved" test -n "$P"
if [ -n "$P" ]; then
    KCTL -n "$NS" logs "$P" -c openrusty-proxy -f --pod-running-timeout=30s \
        > "$TMP/a-shutdown.log" 2>&1 &
    LOG_FOLLOW_PID=$!
    KCTL -n "$NS" delete pod "$P" >/dev/null
    CODE=""
    for _ in $(seq 1 30); do
        CODE="$(KCTL -n "$NS" get pod "$P" \
            -o jsonpath='{.status.containerStatuses[?(@.name=="openrusty-proxy")].state.terminated.exitCode}' \
            2>/dev/null || true)"
        if [ -n "$CODE" ]; then break; fi
        CODE=""; sleep 1
    done
    if [ -n "$CODE" ]; then
        check "S5 sidecar terminated with exit code 0" test "$CODE" = "0"
    else
        echo "NOTE: S5 pod object vanished before exit-code sampling; log assertions carry the check"
    fi
    for _ in $(seq 1 30); do
        if grep -q "shutdown: complete" "$TMP/a-shutdown.log" 2>/dev/null; then break; fi
        sleep 1
    done
    check "S5 shutdown log: phase 1/3 stop accepting" \
        grep -q "shutdown: phase 1/3" "$TMP/a-shutdown.log"
    check "S5 shutdown log: complete" grep -q "shutdown: complete" "$TMP/a-shutdown.log"
    kill "$LOG_FOLLOW_PID" 2>/dev/null || true
    LOG_FOLLOW_PID=""
    KCTL -n "$NS" wait --for=condition=Ready pod -l app=ngx-a --timeout=180s >/dev/null 2>&1 || true
    P2="$(KCTL -n "$NS" get pod -l app=ngx-a --field-selector=status.phase=Running \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
    check "S5 replacement pod ready (new name)" test -n "$P2" -a "$P2" != "$P"
    check "S5 replacement init container exit 0" pod_init_exit0 "$P2"
fi
ROUNDS=$((WINDOW_SECONDS / 5))
[ "$ROUNDS" -ge 1 ] || ROUNDS=1
WIN_ADMIN=0 WIN_SVC=0 ROUND=0
while [ "$ROUND" -lt "$ROUNDS" ]; do
    ROUND=$((ROUND + 1))
    if ! code200 curl-b curl-b http://127.0.0.1:4191/openrusty/ready; then WIN_ADMIN=$((WIN_ADMIN + 1)); fi
    if ! code200 curl-b curl-b http://127.0.0.1:4191/openrusty/live; then WIN_ADMIN=$((WIN_ADMIN + 1)); fi
    if ! code200 curl-b curl-b http://svc-a/; then WIN_SVC=$((WIN_SVC + 1)); fi
    if [ "$ROUND" != "$ROUNDS" ]; then sleep 5; fi
done
check "S5 window: sidecar admin ready+live stayed 200" test "$WIN_ADMIN" -eq 0
check "S5 window: svc-a requests stayed 200" test "$WIN_SVC" -eq 0

# ---- S6. chart demo gate (demo deployment + transparent inbound) ----
if [ "$SKIP_CHART" = 1 ]; then
    echo; echo "== S6. chart demo gate == SKIPPED (--skip-chart)"
else
    echo; echo "== S6. chart demo gate =="
    DEMO_NAME="$(helm template openrusty-demo "$ROOT/deploy/charts/openrusty" \
        --set demo.enabled=true --show-only templates/demo-app.yaml 2>/dev/null \
        | awk '/kind: Deployment/{found=1} found && /name:/{print $2; exit}' || true)"
    check "S6 demo deployment name extracted from chart" test -n "$DEMO_NAME"
    if [ -n "$DEMO_NAME" ]; then
        helm_install() {
            helm upgrade --install openrusty-demo "$ROOT/deploy/charts/openrusty" -n "$NS" \
                --set demo.enabled=true --set "image.repository=${IMAGE_REF%:*}" \
                --set "image.tag=${IMAGE_REF##*:}" --set demo.image=nginx:alpine \
                >/dev/null 2>&1 && HELM_INSTALLED=1
        }
        check "S6 helm release openrusty-demo installed" helm_install
        check "S6 demo deployment $DEMO_NAME Available" wait_avail "$DEMO_NAME"
        DEMO_POD="$(KCTL -n "$NS" get pod -l 'app.kubernetes.io/component=demo' \
            -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)"
        DEMO_IP="$(KCTL -n "$NS" get pod -l 'app.kubernetes.io/component=demo' \
            -o jsonpath='{.items[0].status.podIP}' 2>/dev/null || true)"
        check "S6 demo pod + pod IP resolved" test -n "$DEMO_POD" -a -n "$DEMO_IP"
        check "S6 demo inbound via sidecar (pod IP :80) -> 200" \
            code200 curl-b curl-b "http://$DEMO_IP:80/"
        check "S6 demo init container exit 0" pod_init_exit0 "$DEMO_POD"
    fi
fi

echo
echo "== CHECKS: $((PASS + FAIL)) total =="
echo "== RESULT: $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ]
