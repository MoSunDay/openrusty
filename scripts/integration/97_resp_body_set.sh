# integration drill section 97: pipeline resp_body_set via the kv-probe
# plugin (contract pinned in plugins/kv-probe, wasm already built at
# build/plugins/kv-probe.wasm): `mode=resp-body` writes the response body
# from the plugin and pairs it with a terminal Decision - text+200, same
# text with deny=403, and the text-less empty-204 shape (section 97).

echo "== 97. pipeline resp_body_set (kv-probe) =="
RB_CODE="$(curl -s -o "$TMP/rb-text.out" -w '%{http_code}' --max-time 8 \
    "$GATE/probe?mode=resp-body&text=hello" || true)"
check "resp_body_set: 200 with the exact written body" \
    bash -c "test '$RB_CODE' = 200 && printf '%s' 'kv-probe-resp-body:hello' \
             | cmp -s - '$TMP/rb-text.out'"

RB_DENY_CODE="$(curl -s -o "$TMP/rb-deny.out" -w '%{http_code}' --max-time 8 \
    "$GATE/probe?mode=resp-body&text=hello&deny=403" || true)"
check "resp_body_set: deny=403 keeps the written body" \
    bash -c "test '$RB_DENY_CODE' = 403 && printf '%s' 'kv-probe-resp-body:hello' \
             | cmp -s - '$TMP/rb-deny.out'"

RB_EMPTY_CODE="$(curl -s -o "$TMP/rb-empty.out" -w '%{http_code}' --max-time 8 \
    "$GATE/probe?mode=resp-body" || true)"
check "resp_body_set: no text -> 204 with an empty body" \
    bash -c "test '$RB_EMPTY_CODE' = 204 && test ! -s '$TMP/rb-empty.out'"
