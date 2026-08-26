#!/usr/bin/env bash
#
# install-service.sh - Install OpenRusty gateway + demo echo upstreams as
# systemd services that start now and on boot. Idempotent: safe to re-run;
# re-runs rewrite the unit files and restart all units so new binaries are
# picked up.
#
# Installs:
#   /etc/systemd/system/openrusty.service         (the gateway)
#   /etc/systemd/system/openrusty-echo@.service   (template demo upstream)
#   enables+starts openrusty-echo@9001/9002/9003 and openrusty.service
#
set -euo pipefail

REPO_DIR="/root/openrusty"
GATEWAY_BIN="$REPO_DIR/target/release/openrusty"
ECHO_BIN="$REPO_DIR/target/release/examples/echo_upstream"
CONFIG="$REPO_DIR/config/openrusty.toml"
ECHO_UNIT="/etc/systemd/system/openrusty-echo@.service"
GATEWAY_UNIT="/etc/systemd/system/openrusty.service"
ECHO_INSTANCES=(openrusty-echo@9001 openrusty-echo@9002 openrusty-echo@9003)
ECHO_UNITS=""
for u in "${ECHO_INSTANCES[@]}"; do ECHO_UNITS+="$u.service "; done
ECHO_UNITS="${ECHO_UNITS% }"

fail() { echo "ERROR: $*" >&2; exit 1; }

# --- 1. must run as root (systemd unit installation requires it) -----------
[[ $(id -u) -eq 0 ]] || fail "must run as root (try: sudo bash $0)"

# --- 2. release binaries must exist -----------------------------------------
missing=0
for bin in "$GATEWAY_BIN" "$ECHO_BIN"; do
  if [[ ! -x "$bin" ]]; then
    echo "missing binary: $bin" >&2
    missing=1
  fi
done
if [[ $missing -ne 0 ]]; then
  fail "release binaries not found. Build them first:
  cd $REPO_DIR
  cargo build --release -p openrusty-server
  cargo build --release -p openrusty-server --example echo_upstream"
fi
[[ -f "$CONFIG" ]] || fail "runtime config $CONFIG not found (copy config/openrusty.example.toml)"

# --- 3. write the echo upstream template unit -------------------------------
cat > "$ECHO_UNIT" << UNIT
# OpenRusty demo echo upstream (template). %i is the port / node id.
[Unit]
Description=OpenRusty demo echo upstream on port %i
After=network.target

[Service]
Type=simple
ExecStart=$ECHO_BIN 127.0.0.1:%i node%i
Restart=on-failure

[Install]
WantedBy=multi-user.target
UNIT
echo "wrote $ECHO_UNIT"

# --- 4. write the gateway unit ----------------------------------------------
cat > "$GATEWAY_UNIT" << UNIT
# OpenRusty gateway. WorkingDirectory is required: the config references
# the relative plugin dir "build/plugins".
[Unit]
Description=OpenRusty WASM plugin gateway
After=network.target $ECHO_UNITS
Wants=$ECHO_UNITS

[Service]
Type=simple
WorkingDirectory=$REPO_DIR
ExecStart=$GATEWAY_BIN $CONFIG
Restart=on-failure
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
UNIT
echo "wrote $GATEWAY_UNIT"

# --- 5. reload systemd, enable and (re)start everything ----------------------
systemctl daemon-reload

for unit in "${ECHO_INSTANCES[@]}"; do
  systemctl enable "$unit.service"
  # restart (not start) so re-runs pick up freshly built binaries
  systemctl restart "$unit.service"
done
systemctl enable openrusty.service
systemctl restart openrusty.service

# --- 6. report ---------------------------------------------------------------
echo
echo "unit status (is-enabled / is-active):"
for unit in "${ECHO_INSTANCES[@]}" openrusty; do
  enabled=$(systemctl is-enabled "$unit.service" 2>/dev/null || true)
  active=$(systemctl is-active "$unit.service" 2>/dev/null || true)
  printf '  %-28s %-10s %s\n' "$unit.service" "$enabled" "$active"
done

echo
echo "done. check logs with: journalctl -u openrusty -f"
