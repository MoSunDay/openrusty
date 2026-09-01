#!/usr/bin/env bash
# cluster-e2e: M4 cluster e2e drill - seven assertion groups (G1-G7)
# against a real cluster; the ingress-gateway form of openrusty.
# Sibling of scripts/chart-lint.sh (static helm rendering) and the
# local-netns / local-egress drills; this one needs kubectl + a real
# cluster. Red lines provenance (docs/sidecar.md):
#   G1 preflight -> Security boundaries: get/list/watch on ingresses +
#     secrets; kubeconfig only via $KUBECONFIG -> ~/.kube/config,
#     never printed or logged.
#   G2 deploy/reach -> Deployment surface: helm ingress release
#     (NodePort, or LoadBalancer with recorded fallback) + Lifecycle
#     /openrusty/ready.
#   G3 routes + TLS -> Ingress (Exact vs prefix, host constraint) +
#     TLS termination and SNI; a non-matching host never hits the route.
#   G4 watch -> Watch semantics: fold bounds, stale-serve while the
#     apiserver is blocked, re-list convergence (status ingress node:
#     reconnects/last_rv).
#   G5 conflict -> Conflict policy: a double-claimed (host, path) key
#     or a missing TLS Secret rejects the WHOLE apply; the previous
#     config stays authoritative, generation does not advance.
#   G6 rollback -> Ingress is an enhancement, never a dependency:
#     static baseline returns, ready stays 200, no crashloop.
#   G7 window -> Lifecycle ready/live + the status ingress node:
#     watching true in every sample (drops converge back).
# Modes: --preflight = structural self-check + the G1 checklist with
#   every gap named; G2-G7 print SKIP(cluster-e2e): <named gap>; exit 0.
#   Default = full drill: missing base tooling (kubectl/helm/kubeconfig)
#   -> SKIP + exit 0 (gate convention of the sibling drills);
#   cluster-backed G1 gaps (version, RBAC, image) -> gap list, exit 1.
#   Summary "== cluster-e2e RESULT: PASS n / FAIL m / SKIP k ==";
#   exit 1 iff FAIL > 0.
# Usage: scripts/cluster-e2e.sh [--preflight] [--namespace NS]
#   [--release NAME] [--image REPO:TAG] [--skip-image-check]
#   [--window-min N]. --window-min is for quick local iterations ONLY;
#   the M4 acceptance run MUST use the default 10-minute window.
set -euo pipefail
cd "$(dirname "$0")/.."

CHART=deploy/charts/openrusty FIXTURE=tests/fixtures/inject/deployment.yaml
ING_CLASS=openrusty HOST_NAME=orr-e2e.example.com BACKEND_MARK=orr-cluster-e2e-backend
PRIMARY=orr-e2e-primary ADMIN_LOCAL=41919 ADMIN="http://127.0.0.1:$ADMIN_LOCAL"
LB_WAIT=120 ROUTE_WAIT=120 CUT_SECS=12 WINDOW_POLL=10   # seconds
MODE=full WINDOW_MIN=10 IMAGE="" SKIP_IMAGE=0 NS_ARG=""
RELEASE=orr-cluster-e2e GATE_SVC="" EDGE_PORT=8443 GW_ADDR="" PODSEL=""
KCFG="" CAN_NETPOL=no TUNNEL_PID="" NS="" HELM_DONE=""
PASS=0 FAIL=0 SKIP=0 GAPS=() TMP="$(mktemp -d /tmp/openrusty-clustere2e.XXXXXX)"

while [ $# -gt 0 ]; do
    case "$1" in
    --preflight) MODE=preflight ;;
    --window-min) WINDOW_MIN="$2"; shift ;;
    --image) IMAGE="$2"; shift ;;
    --skip-image-check) SKIP_IMAGE=1 ;;
    --namespace) NS_ARG="$2"; shift ;;
    --release) RELEASE="$2"; shift ;;
    *) echo "cluster-e2e: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
GATE_SVC="$RELEASE-openrusty-ingress"
PODSEL="app.kubernetes.io/instance=$RELEASE,app.kubernetes.io/component=ingress"

cleanup() { # best effort only; never masks an assertion result
    [ -n "$TUNNEL_PID" ] && kill "$TUNNEL_PID" 2>/dev/null || true
    if command -v kubectl >/dev/null 2>&1 && [ -n "$NS" ]; then
        kubectl delete ingress,netpol,secret,svc,deploy,configmap -l app.kubernetes.io/orr-e2e=yes \
            -n "$NS" --ignore-not-found >/dev/null 2>&1 || true
        [ -n "$HELM_DONE" ] && helm uninstall "$RELEASE" -n "$NS" >/dev/null 2>&1 || true
    fi; rm -rf "$TMP"
}
trap cleanup EXIT

check() { # name condition...
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then PASS=$((PASS + 1)); echo "PASS: $name"; else FAIL=$((FAIL + 1)); echo "FAIL: $name"; fi
}
skip_item() { SKIP=$((SKIP + 1)); echo "SKIP(cluster-e2e): $1"; }
skip_groups() { # reason - one SKIP line per cluster group
    local g
    for g in "G2 deploy + reachability" "G3 cluster routing + TLS" "G4 watch resilience" \
        "G5 conflict policy" "G6 removal rollback" "G7 observation window"; do skip_item "$g skipped: $1"; done
}
gap() { GAPS+=("$1"); echo "GAP: $1"; }
fail_stop() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }
summarize() { echo; echo "== cluster-e2e RESULT: PASS $PASS / FAIL $FAIL / SKIP $SKIP =="; }
gap_list() { # gaps joined for the skip reasons
    local out="" g
    for g in "${GAPS[@]+"${GAPS[@]}"}"; do out="$out$g; "; done
    printf '%s' "${out%; }"
}
wait_for() { # timeout_s condition...
    local deadline=$(( $(date +%s) + $1 )); shift
    while [ "$(date +%s)" -lt "$deadline" ]; do "$@" >/dev/null 2>&1 && return 0; sleep 2; done
    return 1
}
status_field() { # path under the status JSON "ingress" node (no jq assumed)
    curl -s --max-time 5 "$ADMIN/openrusty/status" | python3 -c '
import json, sys
n = json.load(sys.stdin).get("ingress", {})
for k in sys.argv[1].split("."): n = n.get(k) if isinstance(n, dict) else None
print("" if n is None else n)' "$1" 2>/dev/null || true
}
svc_field() { kubectl get svc "$GATE_SVC" -n "$NS" -o "jsonpath=$1" 2>/dev/null || true; }
admin_tunnel() { # the admin port 4191 is not exposed by the Service
    [ -n "$TUNNEL_PID" ] && kill "$TUNNEL_PID" 2>/dev/null || true
    kubectl port-forward -n "$NS" "deploy/$GATE_SVC" "127.0.0.1:$ADMIN_LOCAL:4191" >/dev/null 2>&1 &
    TUNNEL_PID=$!
    wait_for 30 bash -c "exec 3<>/dev/tcp/127.0.0.1/$ADMIN_LOCAL"
}
pod_json() { kubectl get pods -n "$NS" -l "$PODSEL" -o "jsonpath=$1" 2>/dev/null || true; }
pod_restarts() { pod_json '{range .items[*]}{range .status.containerStatuses[*]}{.restartCount}{end}{end}'; }
pod_reasons() { pod_json '{range .items[*]}{range .status.containerStatuses[*]}{.state.waiting.reason}{end}{end}'; }
probe_body() { curl -sk --max-time 8 --resolve "$1:$EDGE_PORT:$GW_ADDR" "https://$1:$EDGE_PORT$2" || true; }
route_serves() { probe_body "$HOST_NAME" "$1" | grep -q "$BACKEND_MARK"; }
route_absent() { ! route_serves "$1"; }
sni_subject_matches() {
    echo | timeout 10 openssl s_client -connect "$GW_ADDR:$EDGE_PORT" -servername "$HOST_NAME" \
        2>/dev/null | openssl x509 -noout -subject | grep -q "CN *= *$HOST_NAME" || true
}
gen_advances() { [ -n "$1" ] && [ "$(status_field ingresses.generation)" -gt "$1" ]; }
rv_observable() { [ -n "$(status_field ingresses.last_rv)" ] && [ -n "$(status_field ingresses.reconnects)" ]; }
resolve_ns() { # --namespace, else a namespace key in chart values, else default
    if [ -n "$NS_ARG" ]; then NS="$NS_ARG"; return 0; fi
    NS="$(sed -n 's/^namespace:[[:space:]]*//p' "$CHART/values.yaml" 2>/dev/null | head -1 | tr -d '" ' || true)"; NS="${NS:-default}"
}
write_ingress() { # file name tls_secret path1 type1 [path2 type2]
    local f="$1" name="$2" sec="$3" tls="" paths="" pair
    [ -n "$sec" ] && printf -v tls '  tls:\n  - hosts: [%s]\n    secretName: %s\n' "$HOST_NAME" "$sec" || true
    for pair in "$4 $5" "${6:-} ${7:-}"; do
        [ "$pair" = " " ] && continue
        set -- $pair # path + pathType, word-split on purpose
        printf -v paths '%s      - path: %s\n        pathType: %s\n        backend:\n          service:\n            name: orr-e2e-echo\n            port: {number: 80}\n' "$paths" "$1" "$2"
    done
    printf 'apiVersion: networking.k8s.io/v1\nkind: Ingress\nmetadata: {name: %s, namespace: %s, labels: {app.kubernetes.io/orr-e2e: yes}}\nspec:\n  ingressClassName: %s\n%s  rules:\n  - host: %s\n    http:\n      paths:\n%s' \
        "$name" "$NS" "$ING_CLASS" "$tls" "$HOST_NAME" "$paths" > "$f"
}
write_backend() {
    printf 'apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: orr-e2e-echo, namespace: %s, labels: {app.kubernetes.io/orr-e2e: yes}}\nspec:\n  replicas: 1\n  selector: {matchLabels: {app: orr-e2e-echo}}\n  template:\n    metadata: {labels: {app: orr-e2e-echo}}\n    spec:\n      containers:\n        - name: echo\n          image: hashicorp/http-echo:1.0.0\n          args: ["-text=%s", "-listen=:8080"]\n          readinessProbe: {httpGet: {path: /, port: 8080}, initialDelaySeconds: 2}\n---\napiVersion: v1\nkind: Service\nmetadata: {name: orr-e2e-echo, namespace: %s, labels: {app.kubernetes.io/orr-e2e: yes}}\nspec:\n  selector: {app: orr-e2e-echo}\n  ports: [{name: http, port: 80, targetPort: 8080}]\n' \
        "$NS" "$BACKEND_MARK" "$NS" > "$TMP/backend.yaml"
}
write_cut() { # deny all egress for the gateway pods (stale-serve drill)
    printf 'apiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata: {name: orr-e2e-cut, namespace: %s, labels: {app.kubernetes.io/orr-e2e: yes}}\nspec:\n  podSelector:\n    matchLabels:\n      app.kubernetes.io/name: openrusty\n      app.kubernetes.io/instance: %s\n  policyTypes: [Egress]\n  egress: []\n' \
        "$NS" "$RELEASE" > "$TMP/cut.yaml"
}

g1_preflight() {
    echo "== G1: preflight (docs/sidecar.md: Security boundaries) =="
    check "G1: script parses (bash -n)" bash -n "$0"
    check "G1: chart present ($CHART)" test -f "$CHART/Chart.yaml"
    check "G1: inject fixture present ($FIXTURE)" test -f "$FIXTURE"
    command -v kubectl >/dev/null 2>&1 && check "G1: kubectl in PATH" command -v kubectl || gap "kubectl not found in PATH"
    command -v helm >/dev/null 2>&1 && check "G1: helm in PATH" command -v helm || gap "helm not found in PATH"
    if [ -n "${KUBECONFIG:-}" ] && [ -f "${KUBECONFIG%%:*}" ]; then
        KCFG="${KUBECONFIG%%:*}"; check "G1: kubeconfig resolved via \$KUBECONFIG" test -f "$KCFG"
    elif [ -n "${KUBECONFIG:-}" ]; then
        gap "KUBECONFIG set but its first file is missing: ${KUBECONFIG%%:*}"
    elif [ -f "$HOME/.kube/config" ]; then
        KCFG="$HOME/.kube/config"; check "G1: kubeconfig resolved at ~/.kube/config" test -f "$KCFG"
    else
        gap "no kubeconfig (KUBECONFIG unset and ~/.kube/config absent)"
    fi
    if [ "${#GAPS[@]}" -gt 0 ]; then echo "-- G1: cluster-backed items skipped until the gaps above are fixed"; return 0; fi
    if ! timeout 20 kubectl cluster-info >/dev/null 2>&1; then gap "cluster unreachable (kubectl cluster-info failed)"; return 0; fi
    check "G1: cluster reachable (kubectl cluster-info)" timeout 20 kubectl cluster-info
    local ver major minor v
    ver="$(kubectl version -o jsonpath='{.serverVersion.major} {.serverVersion.minor}' 2>/dev/null | tr -dc '0-9 ' || true)"
    major="${ver%% *}"; minor="$(printf '%s' "${ver#* }" | tr -dc 0-9)"
    if [ -n "$major" ] && { [ "$major" -ge 2 ] || { [ "$major" -eq 1 ] && [ "$minor" -ge 19 ]; }; }; then
        check "G1: server version >= 1.19 (got $major.$minor)" true
    else
        gap "server version undetermined or < 1.19 (jsonpath gave '$ver')"
    fi
    resolve_ns
    echo "-- G1: RBAC target namespace: $NS (--namespace, chart values, or default)"
    rbac() { # verb resource - every miss lands in the operator gap list
        if kubectl auth can-i "$1" "$2" -n "$NS" 2>/dev/null | grep -qx yes; then PASS=$((PASS + 1)); echo "PASS: G1: RBAC $1 $2"
        else FAIL=$((FAIL + 1)); echo "FAIL: G1: RBAC $1 $2"; gap "RBAC: cannot $1 $2 in namespace $NS"; fi
    }
    for v in get list watch; do rbac "$v" ingresses.networking.k8s.io; rbac "$v" secrets; done
    CAN_NETPOL=no
    kubectl auth can-i create networkpolicies.networking.k8s.io -n "$NS" 2>/dev/null | grep -qx yes && CAN_NETPOL=yes
    check "G1: RBAC create networkpolicies (G4 blackout)" test "$CAN_NETPOL" = yes
    if [ "$SKIP_IMAGE" = 1 ]; then
        skip_item "G1 image check: disabled with --skip-image-check"
    elif [ -z "$IMAGE" ]; then
        gap "no --image given: the chart's placeholder tag will never pull"
    elif command -v docker >/dev/null 2>&1 && timeout 60 docker manifest inspect "$IMAGE" >/dev/null 2>&1; then
        check "G1: image resolvable (docker manifest inspect: $IMAGE)" true
    elif kubectl run orr-e2e-imgchk -n "$NS" --image="$IMAGE" --restart=Never --dry-run=client -o name >/dev/null 2>&1; then
        check "G1: image accepted (kubectl run --dry-run=client: $IMAGE)" true
    else
        FAIL=$((FAIL + 1)); echo "FAIL: G1: image not resolvable: $IMAGE"
        gap "image not resolvable (docker/kubectl paths failed): $IMAGE"
    fi
    command -v python3 >/dev/null 2>&1 && check "G1: python3 available (status assertions)" command -v python3 ||
        gap "python3 not found (status assertions need it)"
    return 0
}

g2_deploy() {
    echo "== G2: deploy + reachability (chart ingress release; Lifecycle ready) =="
    echo "-- cleanup on exit: labeled drill resources + helm uninstall $RELEASE (best effort)"
    local imgset=()
    case "$IMAGE" in
    *:*) imgset=(--set "image.repository=${IMAGE%:*}" --set "image.tag=${IMAGE##*:}") ;;
    ?*) imgset=(--set "image.repository=$IMAGE") ;;
    esac
    if ! helm upgrade --install "$RELEASE" "$CHART" -n "$NS" --create-namespace --wait --timeout 5m \
            "${imgset[@]+"${imgset[@]}"}" --set ingress.service.type=LoadBalancer; then
        fail_stop "G2: helm install (ingress release, LoadBalancer)"; return 0
    fi
    HELM_DONE=1
    lb_addr() {
        local a; a="$(svc_field '{.status.loadBalancer.ingress[0].ip}')"; [ -n "$a" ] || a="$(svc_field '{.status.loadBalancer.ingress[0].hostname}')"
        printf '%s' "$a"
    }
    GW_ADDR="$(lb_addr)"
    if [ -n "$GW_ADDR" ] || wait_for "$LB_WAIT" lb_addr; then
        GW_ADDR="$(lb_addr)"
    else
        echo "-- G2: LoadBalancer has no address after ${LB_WAIT}s; downgrading to NodePort"
        helm upgrade --install "$RELEASE" "$CHART" -n "$NS" --wait --timeout 5m "${imgset[@]+"${imgset[@]}"}" \
            --set ingress.service.type=NodePort || { fail_stop "G2: helm upgrade to NodePort"; return 0; }
        GW_ADDR="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="ExternalIP")].address}' 2>/dev/null || true)"
        [ -n "$GW_ADDR" ] || GW_ADDR="$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null || true)"
        EDGE_PORT="$(svc_field '{.spec.ports[0].nodePort}')"
    fi
    check "G2: gateway edge address resolved ($GW_ADDR:$EDGE_PORT)" test -n "$GW_ADDR"
    check "G2: ingress rollout ready" kubectl rollout status "deploy/$GATE_SVC" -n "$NS" --timeout=300s
    if ! admin_tunnel; then fail_stop "G2: admin port 4191 not reachable via port-forward"; return 0; fi
    check "G2: admin port reachable (4191 via port-forward)" test -n "$TUNNEL_PID"
    check "G2: data port reachable (tcp $GW_ADDR:$EDGE_PORT)" \
        wait_for 60 bash -c "exec 3<>/dev/tcp/$GW_ADDR/$EDGE_PORT"
    check "G2: /openrusty/ready is 200" curl -sf --max-time 5 "$ADMIN/openrusty/ready"
    return 0
}

g3_routes() {
    echo "== G3: cluster routing + TLS (Ingress rendering; TLS termination and SNI) =="
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=$HOST_NAME" \
        -keyout "$TMP/tls.key" -out "$TMP/tls.crt" >/dev/null 2>&1 || true
    check "G3: self-signed fixture pair generated" test -s "$TMP/tls.key" -a -s "$TMP/tls.crt"
    if kubectl create secret tls orr-e2e-tls -n "$NS" --cert="$TMP/tls.crt" --key="$TMP/tls.key" \
            --dry-run=client -o yaml | kubectl apply -f - &&
            kubectl label secret orr-e2e-tls -n "$NS" app.kubernetes.io/orr-e2e=yes --overwrite; then
        PASS=$((PASS + 1)); echo "PASS: G3: TLS Secret applied"
    else
        fail_stop "G3: TLS Secret apply"; return 0
    fi
    write_backend
    check "G3: echo backend applied" kubectl apply -f "$TMP/backend.yaml"
    check "G3: echo backend ready" kubectl rollout status deploy/orr-e2e-echo -n "$NS" --timeout=180s
    write_ingress "$TMP/primary.yaml" "$PRIMARY" orr-e2e-tls /exact Exact /pre Prefix
    check "G3: primary Ingress applied (Exact + Prefix, host-constrained)" kubectl apply -f "$TMP/primary.yaml"
    # Chart v1 ships the edge listener plain (tls = false; edge TLS stays
    # with the fronting LB). "TLS termination and SNI" needs tls = true,
    # so the drill patches the rendered ConfigMap and rolls; the chart on
    # disk stays untouched.
    kubectl get configmap "$GATE_SVC-config" -n "$NS" -o jsonpath='{.data.openrusty\.toml}' 2>/dev/null |
        sed 's/tls = false/tls = true/' > "$TMP/tls.toml" || true
    if ! grep -q 'tls = true' "$TMP/tls.toml" 2>/dev/null; then
        fail_stop "G3: could not render a tls = true listener (chart shape changed?)"; return 0
    fi
    kubectl create configmap "$GATE_SVC-config" -n "$NS" --from-file="openrusty.toml=$TMP/tls.toml" \
        -o yaml --dry-run=client | kubectl apply -f - >/dev/null 2>&1 || true
    kubectl label configmap "$GATE_SVC-config" -n "$NS" app.kubernetes.io/orr-e2e=yes --overwrite >/dev/null 2>&1 || true
    kubectl rollout restart "deploy/$GATE_SVC" -n "$NS" >/dev/null 2>&1 || true
    check "G3: tls = true listener live (ConfigMap patch + rollout)" \
        kubectl rollout status "deploy/$GATE_SVC" -n "$NS" --timeout=300s
    if ! admin_tunnel; then fail_stop "G3: admin port-forward after rollout"; return 0; fi
    echo "-- G3: waiting up to ${ROUTE_WAIT}s for the rendered routes"
    wait_for "$ROUTE_WAIT" route_serves /exact || true
    check "G3: SNI handshake serves the adopted certificate" sni_subject_matches
    check "G3: Exact path /exact served by the backend" route_serves /exact
    check "G3: Prefix path /pre served by the backend" route_serves /pre
    check "G3: non-matching host never reaches the route" \
        test "$(probe_body wrong.orr-e2e.invalid /exact | grep -c "$BACKEND_MARK" || true)" = 0
    return 0
}

g4_watch() {
    echo "== G4: watch resilience (Watch semantics: fold, stale-serve, re-list) =="
    write_ingress "$TMP/updated.yaml" "$PRIMARY" orr-e2e-tls /exact Exact /pre2 Prefix
    check "G4: Ingress path update applied" kubectl apply -f "$TMP/updated.yaml"
    check "G4: new path /pre2 serves within the bound" wait_for "$ROUTE_WAIT" route_serves /pre2
    check "G4: old path /pre withdrawn after the update" route_absent /pre
    check "G4: Ingress deleted" kubectl delete ingress "$PRIMARY" -n "$NS" --ignore-not-found
    check "G4: routes withdrawn after the delete" wait_for "$ROUTE_WAIT" route_absent /exact
    if [ "$CAN_NETPOL" != yes ]; then
        skip_item "G4 stale-serve: not allowed to create NetworkPolicies in $NS"
        skip_item "G4 re-list convergence: depends on the NetworkPolicy blackout"
        return 0
    fi
    write_ingress "$TMP/primary.yaml" "$PRIMARY" orr-e2e-tls /exact Exact /pre Prefix
    kubectl apply -f "$TMP/primary.yaml" >/dev/null 2>&1 || true
    wait_for "$ROUTE_WAIT" route_serves /pre || true
    write_cut
    if ! kubectl apply -f "$TMP/cut.yaml" >/dev/null 2>&1; then skip_item "G4 stale-serve: NetworkPolicy apply failed"; return 0; fi
    echo "-- G4: apiserver blackout for ${CUT_SECS}s (stale-serve expected)"
    sleep "$CUT_SECS"
    check "G4: stale-serve - /pre still served while the apiserver is blocked" route_serves /pre
    check "G4: blackout policy removed" kubectl delete netpol orr-e2e-cut -n "$NS" --ignore-not-found
    local gen_before; gen_before="$(status_field ingresses.generation)"
    kubectl -n "$NS" annotate ingress "$PRIMARY" orr-e2e-touch="$(date +%s)" --overwrite >/dev/null 2>&1 || true
    check "G4: re-list converges (generation advances after a touch)" \
        wait_for "$ROUTE_WAIT" gen_advances "$gen_before"
    check "G4: watching=true after recovery" test "$(status_field watching)" = True
    check "G4: fresh LIST on record (last_success_age_ms < 60000)" \
        test "$(status_field ingresses.last_success_age_ms)" -lt 60000
    check "G4: reconnects/last_rv observable on the status ingress node" rv_observable
    return 0
}

g5_conflict() {
    echo "== G5: conflict policy (a claimed key rejects the whole apply) =="
    local gen; gen="$(status_field ingresses.generation)"
    # Same (host, path) key as the primary Ingress plus a path unique to
    # this object: a valid merge would have to serve /collide-only.
    write_ingress "$TMP/collide.yaml" orr-e2e-collide orr-e2e-tls /pre Prefix /collide-only Prefix
    check "G5: colliding Ingress accepted by the apiserver" kubectl apply -f "$TMP/collide.yaml"
    sleep 8 # two debounce windows; a rejected apply never renders
    check "G5: previous route still authoritative (/pre serves)" route_serves /pre
    check "G5: /collide-only never appears (whole apply rejected)" route_absent /collide-only
    check "G5: status generation did not advance" test "$(status_field ingresses.generation)" = "$gen"
    write_ingress "$TMP/ghost.yaml" orr-e2e-ghost orr-e2e-missing-tls /ghost Prefix
    check "G5: missing-Secret Ingress accepted by the apiserver" kubectl apply -f "$TMP/ghost.yaml"
    sleep 8
    check "G5: /ghost never appears (apply rejected again)" route_absent /ghost
    check "G5: generation still does not advance" test "$(status_field ingresses.generation)" = "$gen"
    check "G5: /pre still serves after both rejections" route_serves /pre
    kubectl delete ingress orr-e2e-collide orr-e2e-ghost -n "$NS" --ignore-not-found >/dev/null 2>&1 || true
    return 0
}

g6_rollback() {
    echo "== G6: removal rollback (Ingress is an enhancement, never a dependency) =="
    local restarts_before ready_miss=0 elapsed=0; restarts_before="$(pod_restarts)"
    kubectl delete ingress "$PRIMARY" -n "$NS" --ignore-not-found >/dev/null 2>&1 || true
    while [ "$elapsed" -lt 30 ]; do
        curl -sf --max-time 5 "$ADMIN/openrusty/ready" >/dev/null 2>&1 || ready_miss=$((ready_miss + 1))
        sleep 2; elapsed=$((elapsed + 2))
    done
    check "G6: /openrusty/ready stayed 200 during teardown (0 misses)" test "$ready_miss" = 0
    check "G6: removed route no longer serves" wait_for "$ROUTE_WAIT" route_absent /exact
    check "G6: no pod restarts during the rollback" test "$(pod_restarts)" = "$restarts_before"
    check "G6: no CrashLoopBackOff" test "$(pod_reasons | grep -c CrashLoopBackOff || true)" = 0
    return 0
}

g7_window() {
    echo "== G7: observation window (${WINDOW_MIN}min; M4 acceptance: the default 10) =="
    # Put content back so the window observes a live watch, not an idle one.
    write_ingress "$TMP/primary.yaml" "$PRIMARY" orr-e2e-tls /exact Exact /pre Prefix
    kubectl apply -f "$TMP/primary.yaml" >/dev/null 2>&1 || true
    wait_for "$ROUTE_WAIT" route_serves /exact || true
    local t0 restarts0 samples=0 bad_ready=0 bad_live=0 bad_watch=0; restarts0="$(pod_restarts)"; t0="$(date +%s)"
    while [ "$(( $(date +%s) - t0 ))" -lt "$((WINDOW_MIN * 60))" ]; do
        samples=$((samples + 1))
        curl -sf --max-time 5 "$ADMIN/openrusty/ready" >/dev/null 2>&1 || bad_ready=$((bad_ready + 1))
        curl -sf --max-time 5 "$ADMIN/openrusty/live" >/dev/null 2>&1 || bad_live=$((bad_live + 1))
        [ "$(status_field watching)" = True ] || bad_watch=$((bad_watch + 1))
        sleep "$WINDOW_POLL"
    done
    check "G7: ready 200 in all $samples samples" test "$bad_ready" = 0
    check "G7: live 200 in all $samples samples" test "$bad_live" = 0
    check "G7: watching=true in all $samples samples (any drop converged back)" test "$bad_watch" = 0
    check "G7: pod restart count did not grow" test "$(pod_restarts)" = "$restarts0"
    check "G7: status red-line clean (last_success_age_ms < 60000)" \
        test "$(status_field ingresses.last_success_age_ms)" -lt 60000
    return 0
}

echo "cluster-e2e: mode=$MODE release=$RELEASE image=${IMAGE:-<chart default>} window=${WINDOW_MIN}min"
g1_preflight || true
if [ "${#GAPS[@]}" -gt 0 ] &&
    { ! command -v kubectl >/dev/null 2>&1 || ! command -v helm >/dev/null 2>&1 || [ -z "$KCFG" ]; }; then
    echo "-- base tooling missing on this machine: skip, gate convention of the sibling drills"
    skip_groups "$(gap_list)"
elif [ "$MODE" = preflight ]; then
    REASON="$(gap_list)"
    [ -n "$REASON" ] || REASON="preflight mode (cluster groups out of scope)"
    skip_groups "$REASON"
elif [ "${#GAPS[@]}" -gt 0 ]; then
    echo "PREFLIGHT GAPS (fix these, then rerun):"
    for g in "${GAPS[@]}"; do echo "  - $g"; done
    summarize
    exit 1
else
    g2_deploy || true
    g3_routes || true
    g4_watch || true
    g5_conflict || true
    g6_rollback || true
    g7_window || true
fi
summarize
[ "$FAIL" -eq 0 ]
