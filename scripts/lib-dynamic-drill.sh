#!/usr/bin/env bash
# Dynamic-WASM-API drill for scripts/integration.sh (§31). This file is a
# library: SOURCE it, never execute it. The drill assumes the caller's
# globals (check, GATE, TMP, ROOT) and that the gateway was booted with a
# [dynamic] section pointing at $TMP/dynamic, holding echo.wasm +
# reverse.wasm copied from build/plugins (never from build/plugins
# directly: the endpoint must not serve the pipeline plugins), plus a
# [dynamic.settings.echo] greeting = "hi".

# Only meaningful when executed: point at the library, don't run it.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    echo "lib-dynamic-drill.sh is a library; source it from integration.sh" >&2
    exit 64
fi

# Exercise POST /api/v1/dynamic/{name}: settings plumbing, module-set
# headers, error codes, the 1 MiB body cap, metrics, replace-without-
# reload (stat-driven module cache), warm cache across reloads, and the
# proxy plane staying untouched.
dynamic_drill() {
    check "dynamic: echo answers greeting-joined body" bash -c "curl -s -d hello --max-time 8 '$GATE/api/v1/dynamic/echo' | grep -qx 'hi: hello'"
    check "dynamic: module headers win over defaults" bash -c "curl -s -D - -o /dev/null --max-time 8 -d hello '$GATE/api/v1/dynamic/echo' | grep -qi '^x-dynamic: echo' && curl -s -D - -o /dev/null --max-time 8 -d hello '$GATE/api/v1/dynamic/echo' | grep -qi '^content-type: application/octet-stream'"
    check "dynamic: bodyless request falls back to module default" bash -c "curl -s -X POST --max-time 8 '$GATE/api/v1/dynamic/echo' | grep -qx 'hi: dynamic-echo'"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 "$GATE/api/v1/dynamic/echo" || true)"
    check "dynamic: GET rejected with 405" test "$CODE" = "405"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 8 -X POST "$GATE/api/v1/dynamic/unknown" || true)"
    check "dynamic: unknown module 404" test "$CODE" = "404"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' --path-as-is --max-time 8 -X POST "$GATE/api/v1/dynamic/..%2Fetc" || true)"
    check "dynamic: traversal name rejected 400" test "$CODE" = "400"
    CODE="$(head -c 1048577 /dev/zero | curl -s -o /dev/null -w '%{http_code}' --max-time 8 --data-binary @- "$GATE/api/v1/dynamic/echo" || true)"
    check "dynamic: body over the 1 MiB cap rejected 413" test "$CODE" = "413"
    check "dynamic: requests counted in metrics" bash -c "curl -s --max-time 5 '$GATE/openrusty/metrics' | grep -q 'openrusty_dynamic_requests_total{module=\"echo\",code=\"200\"}'"
    # Replace semantics: overwrite echo.wasm with the reverse module and
    # ask again immediately -- the stat-driven compile cache keys on
    # mtime+size, so the new module answers on the very next request, no
    # reload. The KV-survival-across-replacement case is covered by the
    # openrusty-wasm unit tests (dynamic_tests.rs).
    cp "$ROOT/build/plugins/dynamic-reverse.wasm" "$TMP/dynamic/echo.wasm"
    check "dynamic: replaced module answers on the next request" bash -c "curl -s -d hello --max-time 8 '$GATE/api/v1/dynamic/echo' | grep -qx 'olleh'"
    check "dynamic: 200 parallel posts all succeed" python3 - "$GATE" <<'PY'
import sys, threading, urllib.request
gate = sys.argv[1]
ok = [0]
lock = threading.Lock()
def fire():
    try:
        req = urllib.request.Request(gate + "/api/v1/dynamic/reverse", data=b"abc", method="POST")
        with urllib.request.urlopen(req, timeout=30) as resp:
            assert resp.read() == b"cba"
            with lock:
                ok[0] += 1
    except Exception:
        pass
threads = [threading.Thread(target=fire) for _ in range(200)]
for t in threads:
    t.start()
for t in threads:
    t.join()
sys.exit(0 if ok[0] == 200 else 1)
PY
    # Reload with an UNCHANGED [dynamic] section keeps the live registry
    # (warm stat cache): the endpoint keeps serving the replaced module.
    curl -s -X POST --max-time 10 "$GATE/openrusty/reload" > /dev/null || true
    check "dynamic: reload with unchanged section keeps the live module" bash -c "curl -s -d hello --max-time 8 '$GATE/api/v1/dynamic/echo' | grep -qx 'olleh'"
    check "dynamic: reverse module direct" bash -c "curl -s -d abc --max-time 8 '$GATE/api/v1/dynamic/reverse' | grep -qx 'cba'"
    check "dynamic: proxy plane unaffected" bash -c "curl -s --max-time 5 '$GATE/echo' | grep -q node"
}
