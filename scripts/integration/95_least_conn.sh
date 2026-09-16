# integration drill section 95: least_conn balancing — concurrent held
# requests spread over peers by in-flight/weight (section 34).

echo "== 34. least_conn spreads concurrent load =="
# No `task=` query key: without one the kv-scheduler plugin declines the
# balancer phase, so the gateway's own least_conn policy makes the picks.
# (The sed also flips the late-drill timeout-retry/tls-up upstreams; both
# are swrr and restored below, and their sections have already run.)
sed -i 's/balancer = "swrr"/balancer = "least_conn"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
check "least_conn config reloads" wait_healthy 3 20

# 12 overlapping held requests: each pick raises the chosen peer's
# in-flight gauge until the response lands, so the next one goes
# elsewhere. /slow holds for ms= but must stay under section 13's
# /slow route timeout of 300ms, hence ms=150.
seq 1 12 | xargs -P 12 -I{} curl -s -o "$TMP/lc{}.json" -w '%{http_code}\n' --max-time 10 "$GATE/slow?ms=150" > "$TMP/lc_codes.txt"
check "least_conn: 12 held requests all succeed" test "$(grep -c '^200$' "$TMP/lc_codes.txt")" -eq 12
LC_NODES="$(for f in "$TMP"/lc*.json; do cat "$f"; echo; done |
    python3 -c 'import sys,json; print(len({json.loads(line)["node"] for line in sys.stdin if line.strip()}))' 2>/dev/null || echo 0)"
check "least_conn: concurrent load spreads over >=2 peers" test "${LC_NODES:-0}" -ge 2

# Restore the drill's steady state (swrr) and confirm all peers answer.
sed -i 's/balancer = "least_conn"/balancer = "swrr"/' "$TMP/openrusty.toml"
curl -s -X POST --max-time 10 $GATE/openrusty/reload > /dev/null || true
check "least_conn: swrr restored (3 healthy)" wait_healthy 3 20
