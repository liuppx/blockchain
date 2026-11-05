#!/usr/bin/env bash
# zhixing — one-command local single-node network + block explorer.
#
#   ./cmd            # start (build if needed) → node + explorer, print URLs
#   ./cmd up         # same as ./cmd
#   ./cmd status     # chain height / supply via RPC
#   ./cmd logs       # tail the node log
#   ./cmd down       # stop node + explorer
#   ./cmd reset      # stop + wipe .localnet (fresh chain + new keys)
#
# Single validator (id 21, quorum 1 ⇒ self-commits), heartbeat blocks so height ticks with
# no load. State in .localnet/ (gitignored). RPC :9221, metrics :9121, explorer :8080.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
BIN="$ROOT/node/target/release/node"
D="$ROOT/.localnet"
PW="localdev"                 # local-only keystore password (never for production)
RPC_PORT=9221; METRICS_PORT=9121; P2P_PORT=9021; EXPLORER_PORT=8080
export ZHIXING_KEYSTORE_PASSWORD="$PW"

build() {
  if [ ! -x "$BIN" ]; then echo "building release binary (first run)…"; (cd "$ROOT/node" && cargo build --release); fi
}

mint() { "$BIN" keygen --encrypt --out "$1" | awk '$1=="pubkey"{print $2}'; }

gen() {
  mkdir -p "$D/keys" "$D/data"; chmod 700 "$D"
  echo "generating single-node genesis + config in .localnet/ …"
  local vpk; vpk="$(mint "$D/keys/val21.json")"
  local a1 a2 a3; a1="$(mint "$D/keys/acct1.json")"; a2="$(mint "$D/keys/acct2.json")"; a3="$(mint "$D/keys/acct3.json")"
  {
    echo "# local single-node dev genesis (M200 calibration values)"
    echo "base_emission_micro    = 8000000"
    echo "decay_half_life_blocks = 0"
    echo "slash_bps              = 10000"
    echo "timestamp_days         = 0.0"
    for pk in "$a1:1" "$a2:2" "$a3:3"; do
      echo; echo "[[accounts]]"; echo "id = ${pk##*:}"; echo "balance_micro = 30000000"; echo "pubkey_hex = \"${pk%%:*}\""
    done
    for id in 10 11 12; do echo; echo "[[reviewers]]"; echo "id = $id"; echo "weight = 1.0"; done
    echo; echo "[[validators]]"; echo "id = 21"; echo "pubkey_hex = \"$vpk\""; echo "power = 1"
  } > "$D/genesis.toml"
  {
    echo "genesis = \"$D/genesis.toml\""
    echo; echo "[node]"; echo "id = 21"; echo "listen = \"127.0.0.1:$P2P_PORT\""; echo "data_dir = \"$D/data\""
    echo; echo "[validator]"; echo "enabled = true"; echo "encrypted_keystore_path = \"$D/keys/val21.json\""
    echo; echo "[consensus]"; echo "block_interval_ms = 1000"; echo "create_empty_blocks = true"
    echo; echo "[metrics]"; echo "enabled = true"; echo "listen = \"127.0.0.1:$METRICS_PORT\""; echo "enable_tls = false"
    echo; echo "[rpc]"; echo "enabled = true"; echo "listen = \"127.0.0.1:$RPC_PORT\""; echo "enable_tls = false"
  } > "$D/node.toml"
  "$BIN" check-config --config "$D/node.toml" >/dev/null && echo "  check-config ok"
  "$BIN" genesis-hash --config "$D/node.toml" | sed 's/^/  /'
}

running() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }

up() {
  build
  [ -f "$D/node.toml" ] || gen
  if running "$D/node.pid"; then echo "node already running (pid $(cat "$D/node.pid"))";
  else
    nohup "$BIN" run --config "$D/node.toml" >> "$D/node.log" 2>&1 &
    echo $! > "$D/node.pid"; echo "node started (pid $!)  log: .localnet/node.log"
  fi
  if command -v node >/dev/null 2>&1; then
    if running "$D/explorer.pid"; then echo "explorer already running (pid $(cat "$D/explorer.pid"))";
    else
      PORT="$EXPLORER_PORT" NODE_RPC="http://127.0.0.1:$RPC_PORT" \
        nohup node "$ROOT/tools/explorer/server.js" >> "$D/explorer.log" 2>&1 &
      echo $! > "$D/explorer.pid"; echo "explorer started (pid $!)"
    fi
    echo
    echo "  explorer : http://localhost:$EXPLORER_PORT"
  else
    echo "  (node.js not found — open tools/explorer/public/index.html and set RPC to http://127.0.0.1:$RPC_PORT)"
  fi
  echo "  node RPC : http://127.0.0.1:$RPC_PORT   (try: ./cmd status)"
  echo "  metrics  : http://127.0.0.1:$METRICS_PORT/metrics"
}

down() {
  for role in explorer node; do
    local pf="$D/$role.pid"
    if running "$pf"; then kill -TERM "$(cat "$pf")" 2>/dev/null || true
      for _ in $(seq 1 50); do running "$pf" || break; sleep 0.1; done
      echo "$role stopped"
    fi
    rm -f "$pf"
  done
}

status() {
  if ! running "$D/node.pid"; then echo "node not running (./cmd up)"; exit 1; fi
  local h; h="$(curl -s --max-time 2 "http://127.0.0.1:$RPC_PORT/height" || true)"
  local s; s="$(curl -s --max-time 2 "http://127.0.0.1:$RPC_PORT/supply" || true)"
  echo "height : $h"
  echo "supply : $s"
}

case "${1:-up}" in
  up|"") up ;;
  down) down ;;
  reset) down; rm -rf "$D"; echo "wiped .localnet" ;;
  status) status ;;
  logs) tail -f "$D/node.log" ;;
  *) echo "usage: ./cmd [up|down|reset|status|logs]" >&2; exit 2 ;;
esac
