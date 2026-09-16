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

    # ---- registration face + {method,path} bindings -----------------------
    # All of /openrusty/* sits behind the [admin] token when one is set;
    # this boot config sets none, so the face is exercised directly.
    # (a) boot-time [[dynamic.routes]] enforcement.
    check "dynamic: listing shows the config binding" bash -c "curl -s --max-time 5 '$GATE/openrusty/dynamic' | python3 -c 'import sys,json; d=json.load(sys.stdin); assert {\"echo\", \"reverse\"} <= set(d[\"modules\"]); assert any(r[\"module\"]==\"echo\" and r[\"method\"]==\"GET\" and r[\"path\"]==\"/bound/*\" for r in d[\"routes\"])'"
    check "dynamic: config-bound prefix serves below the base" bash -c "curl -s --max-time 8 '$GATE/bound/x' | grep -qx 'hi: dynamic-echo'"
    check "dynamic: config-bound prefix serves the bare base" bash -c "curl -s --max-time 8 '$GATE/bound' | grep -qx 'hi: dynamic-echo'"
    check "dynamic: prefix does not match sibling paths" bash -c "! curl -s --max-time 8 '$GATE/boundfoo' | grep -q 'dynamic-echo'"
    # (b) PUT stores + binds in one call; bindings win over [[routes]]
    # prefixes (the catch-all / would otherwise proxy these).
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$ROOT/build/plugins/dynamic-reverse.wasm" --max-time 10 "$GATE/openrusty/dynamic/reg?method=post&path=/__reg" || true)"
    check "dynamic: PUT stores and binds in one call" test "$CODE" = "200"
    check "dynamic: bound route dispatches the module" bash -c "curl -s -d abc -X POST --max-time 8 '$GATE/__reg' | grep -qx cba"
    check "dynamic: binding is method-scoped" bash -c "! curl -s --max-time 8 '$GATE/__reg' | grep -qx cba"
    # (c) bind-only form (empty body) + slot replacement semantics.
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --max-time 8 "$GATE/openrusty/dynamic/echo?method=get&path=/__echo2" || true)"
    check "dynamic: bind-only PUT reuses the stored artifact" test "$CODE" = "200"
    check "dynamic: bind-only route serves the module" bash -c "curl -s --max-time 8 '$GATE/__echo2' | grep -qx 'hi: dynamic-echo'"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --max-time 8 "$GATE/openrusty/dynamic/echo?method=post&path=/__slot" || true)"
    check "dynamic: rebinding a base replaces the earlier shape" test "$CODE" = "200"
    check "dynamic: replaced exact binding no longer matches deep paths" bash -c "! curl -s -d abc -X POST --max-time 8 '$GATE/__slot/deep' | grep -qx cba"
    check "dynamic: replaced binding still answers the exact path" bash -c "curl -s -d abc -X POST --max-time 8 '$GATE/__slot' | grep -qx 'hi: abc'"
    # (d) rejection paths: bad name, partial params, unknown module, garbage bytes.
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$ROOT/build/plugins/dynamic-reverse.wasm" --max-time 8 "$GATE/openrusty/dynamic/.hidden?method=get&path=/x" || true)"
    check "dynamic: PUT rejects invalid module names" test "$CODE" = "400"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @"$ROOT/build/plugins/dynamic-reverse.wasm" --max-time 8 "$GATE/openrusty/dynamic/reg?method=post" || true)"
    check "dynamic: PUT rejects partial bind params" test "$CODE" = "400"
    CODE="$(curl -s -o /dev/null -w '%{http_code}' -X PUT --max-time 8 "$GATE/openrusty/dynamic/nosuch?method=get&path=/x" || true)"
    check "dynamic: bind-only against missing artifact is 404" test "$CODE" = "404"
    CODE="$(printf 'not a wasm module' | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- --max-time 8 "$GATE/openrusty/dynamic/garbage" || true)"
    check "dynamic: PUT rejects non-module bytes" test "$CODE" = "400"
    check "dynamic: dispatched requests counted per module" bash -c "curl -s --max-time 5 '$GATE/openrusty/metrics' | grep -q 'openrusty_dynamic_requests_total{module=\"reg\",code=\"200\"}'"
    # (e) DELETE drops artifact + bindings; dispatch stops immediately.
    check "dynamic: DELETE reports the unbound count" bash -c "curl -s -X DELETE --max-time 8 '$GATE/openrusty/dynamic/reg' | grep -q '\"unbound\":1'"
    check "dynamic: unbound path no longer serves the module" bash -c "! curl -s -d abc -X POST --max-time 8 '$GATE/__reg' | grep -qx cba"
    check "dynamic: second DELETE is 404" test "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE --max-time 8 "$GATE/openrusty/dynamic/reg" || true)" = "404"
    # (f) reload keeps runtime bindings and re-enforces config ones.
    curl -s -X POST --max-time 10 "$GATE/openrusty/reload" > /dev/null || true
    check "dynamic: reload keeps runtime-added bindings" bash -c "curl -s --max-time 8 '$GATE/__echo2' | grep -qx 'hi: dynamic-echo'"
    check "dynamic: reload re-enforces config bindings" bash -c "curl -s --max-time 8 '$GATE/bound/x' | grep -qx 'hi: dynamic-echo'"
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
