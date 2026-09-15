#!/usr/bin/env bash
# chart-lint: helm-render deploy/charts/openrusty and assert the shape.
#
# Six releases (default values / demo.enabled=true / ingress Service
# type=NodePort / rbac.create=false / rbac.clusterWide=true / explicit
# watchNamespaces) are templated and checked by
# chart_lint_checks.py, plus
# an inject-CLI consistency drill: `openrusty inject` runs over the
# shared fixture and the output must match the chart's static sidecar
# shape (ports, proxy UID, init flags, config mounts, TOML body).
# Everything renders locally; no cluster, no kubeconfig.
set -euo pipefail
cd "$(dirname "$0")/.."
CHART=deploy/charts/openrusty
FIXTURE=tests/fixtures/inject/deployment.yaml
BIN=target/debug/openrusty
HELM_VERSION=3.16.4
TMP="$(mktemp -d /tmp/openrusty-chartlint.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Helm is the only external tool. Missing: try one network install into
# /usr/local/bin; if that fails too, SKIP loudly instead of passing.
ensure_helm() {
    if command -v helm >/dev/null 2>&1; then return 0; fi
    echo "chart-lint: helm not found; installing v${HELM_VERSION} to /usr/local/bin"
    local os=linux arch
    case "$(uname -m)" in
        x86_64) arch=amd64 ;;
        aarch64 | arm64) arch=arm64 ;;
        *) echo "chart-lint: unsupported arch $(uname -m)"; return 1 ;;
    esac
    local tgz="helm-v${HELM_VERSION}-${os}-${arch}.tar.gz"
    curl -fsSL "https://get.helm.sh/${tgz}" -o "$TMP/$tgz" &&
        tar -xzf "$TMP/$tgz" -C "$TMP" &&
        install -m 0755 "$TMP/${os}-${arch}/helm" /usr/local/bin/helm
}

if ! ensure_helm; then
    echo "SKIP(chart-lint): helm unavailable (not installed, download failed)"
    exit 0
fi

render() { # name set-args...
    local name="$1"; shift
    helm template "$name" "$CHART" "$@" > "$TMP/$name.yaml"
}
render defaults
render demo --set demo.enabled=true
render nodeport --set ingress.service.type=NodePort
render rbacoff --set rbac.create=false
render wide --set rbac.clusterWide=true
render watchns --set 'ingress.watchNamespaces={alpha,beta}'

# The consistency drill needs the inject CLI; build it once if absent.
if [ ! -x "$BIN" ]; then
    echo "chart-lint: building the openrusty binary"
    cargo build -q -p openrusty-server
fi
"$BIN" inject < "$FIXTURE" > "$TMP/injected.yaml"

# YAML assertions need pyyaml; try a quiet install, else degrade to a
# compact grep pass over the rendered releases (never a silent pass).
run_checks() {
    if python3 -c 'import yaml' 2>/dev/null; then
        python3 scripts/chart_lint_checks.py "$TMP/defaults.yaml" \
            "$TMP/demo.yaml" "$TMP/nodeport.yaml" "$TMP/rbacoff.yaml" \
            "$TMP/wide.yaml" "$TMP/watchns.yaml" "$TMP/injected.yaml"
        return
    fi
    echo "chart-lint: pyyaml missing; trying pip install"
    if pip3 install --quiet pyyaml 2>/dev/null && python3 -c 'import yaml' 2>/dev/null; then
        python3 scripts/chart_lint_checks.py "$TMP/defaults.yaml" \
            "$TMP/demo.yaml" "$TMP/nodeport.yaml" "$TMP/rbacoff.yaml" \
            "$TMP/wide.yaml" "$TMP/watchns.yaml" "$TMP/injected.yaml"
        return
    fi
    echo "chart-lint: no pyyaml; grep fallback (shape-only)"
    grep_fallback
}

grep_fallback() {
    local pass=0 fail=0
    expect() { # file pattern
        if grep -q "$2" "$1"; then pass=$((pass + 1)); echo "PASS: $2"
        else fail=$((fail + 1)); echo "FAIL: $2 (in $1)"; fi
    }
    for f in "$TMP/demo.yaml" "$TMP/injected.yaml"; do
        expect "$f" "name: openrusty-init"
        expect "$f" "name: openrusty-proxy"
    done
    for f in "$TMP/defaults.yaml" "$TMP/nodeport.yaml"; do
        expect "$f" "name: openrusty-proxy"
        expect "$f" "name: .*-egress-gateway-config"
    done
    expect "$TMP/defaults.yaml" "type: LoadBalancer"
    expect "$TMP/defaults.yaml" "kind: Role"
    expect "$TMP/nodeport.yaml" "type: NodePort"
    expect "$TMP/demo.yaml" 'config.openrusty.io/skip-inbound-ports: "9090,15002"'
    expect "$TMP/injected.yaml" "name: echo-openrusty-config"
    echo "chart-lint (grep): $pass passed, $fail failed"
    [ "$fail" -eq 0 ]
}

run_checks
