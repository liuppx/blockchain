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
mint_seed() { "$BIN" keygen --out "$1" | awk '$1=="pubkey"{print $2}'; }  # plaintext seed (for encode-tx)

gen() {
  mkdir -p "$D/keys" "$D/data"; chmod 700 "$D"
  echo "generating single-node genesis + config in .localnet/ …"
  local vpk; vpk="$(mint "$D/keys/val21.json")"
  # Accounts as plaintext seed files so `./cmd submit` can author+sign txs (encode-tx needs a seed).
  local a1 a2 a3; a1="$(mint_seed "$D/keys/acct1.seed")"; a2="$(mint_seed "$D/keys/acct2.seed")"; a3="$(mint_seed "$D/keys/acct3.seed")"
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

# Author + submit an ACCEPTED knowledge submission from an account (default 1), so the
# explorer shows real content: a non-empty block → a minted cNFT + a cognitive-graph node +
# the author's earnings. Embedding is a unit vector that rotates by nonce to stay novel
# (novelty drives ΔK > 0 ⇒ accept). Reviews/replication mirror the demo's accepted recipe.
submit() {
  running "$D/node.pid" || { echo "node not running (./cmd up)"; exit 1; }
  local acct="${1:-1}"; local seed="$D/keys/acct${acct}.seed"
  [ -f "$seed" ] || { echo "no seed for account $acct (./cmd reset to regenerate)"; exit 1; }
  local nf="$D/nonce.$acct"; local n; n="$(cat "$nf" 2>/dev/null || echo 0)"
  local dim=$(( n % 8 )); local emb=""
  for i in 0 1 2 3 4 5 6 7; do [ "$i" = "$dim" ] && emb="$emb${emb:+,}1" || emb="$emb${emb:+,}0"; done
  "$BIN" encode-tx --key-file "$seed" --out "$D/tx.bin" --author "$acct" --domain 1 \
    --embedding "$emb" --review 10:0.9 --review 11:0.9 --review 12:0.85 \
    --repl-success 3 --repl-total 3 --timestamp-days 1.0 --stake 2000000 --fee 0 --nonce "$n" >/dev/null
  if "$BIN" submit-tx --config "$D/node.toml" --tx "$D/tx.bin"; then
    echo $((n + 1)) > "$nf"
    echo "→ submitted from account $acct (nonce $n). Watch http://localhost:$EXPLORER_PORT —"
    echo "  within a block or two: a new tx, a minted cNFT, and a cognitive-graph node."
  else
    echo "submit failed (is the RPC up? ./cmd status)"; exit 1
  fi
}

# M228: governance demo. propose/vote author+sign a gov op (encode-gov-*) and POST it to the
# node's /gov/* endpoint; gov shows the live proposal list. NOTE: a proposal *passing* needs
# voters carrying stake (bonds) AND reputation (reviewer) weight — the default demo accounts
# have neither, so a proposal will REGISTER and record votes but not pass (correct, safe
# governance behavior). It still demonstrates the full authoring → submit → read path.
gov_submit() {  # $1=endpoint-path  $2=opfile  $3=nonce-file
  local out; out="$("$BIN" rpc --config "$D/node.toml" --path "$1" --post "$2" 2>&1)"; echo "$out"
  # success returns the op hash (bare 64-hex in text, or {"hash":…} in json); bump nonce then.
  if echo "$out" | grep -qiE '[0-9a-f]{64}|"hash"'; then
    echo $(( $(cat "$3" 2>/dev/null || echo 0) + 1 )) > "$3"
  fi
}
propose() {
  running "$D/node.pid" || { echo "node not running (./cmd up)"; exit 1; }
  local acct="${1:-1}"; local param="${2:-base_emission=9000000}"; local period="${3:-5}"
  local seed="$D/keys/acct${acct}.seed"; [ -f "$seed" ] || { echo "no seed for account $acct"; exit 1; }
  local nf="$D/nonce.$acct"; local n; n="$(cat "$nf" 2>/dev/null || echo 0)"
  "$BIN" encode-gov-proposal --key-file "$seed" --out "$D/gov.bin" --proposer "$acct" \
    --param "$param" --voting-period "$period" --nonce "$n" >/dev/null
  gov_submit /gov/proposal "$D/gov.bin" "$nf"
  echo "→ proposal from account $acct ($param, voting_period=$period). It registers on the next"
  echo "  block; see the explorer Governance page or ./cmd gov. (Won't pass without weighted voters.)"
}
vote() {
  running "$D/node.pid" || { echo "node not running (./cmd up)"; exit 1; }
  local acct="${1:-1}"; local pid="${2:-}"; local approve="${3:-true}"
  [ -n "$pid" ] || { echo "usage: ./cmd vote [acct] <proposal-id> [true|false]"; exit 1; }
  local seed="$D/keys/acct${acct}.seed"; local nf="$D/nonce.$acct"; local n; n="$(cat "$nf" 2>/dev/null || echo 0)"
  "$BIN" encode-gov-vote --key-file "$seed" --out "$D/gov.bin" --proposal-id "$pid" \
    --voter "$acct" --approve "$approve" --nonce "$n" >/dev/null
  gov_submit /gov/vote "$D/gov.bin" "$nf"
  echo "→ vote ($approve) on proposal $pid from account $acct."
}
gov() { curl -s --max-time 2 "http://127.0.0.1:$RPC_PORT/proposals" || true; echo; }

case "${1:-up}" in
  up|"") up ;;
  down) down ;;
  reset) down; rm -rf "$D"; echo "wiped .localnet" ;;
  status) status ;;
  submit) submit "${2:-1}" ;;
  propose) propose "${2:-1}" "${3:-base_emission=9000000}" "${4:-5}" ;;
  vote) vote "${2:-1}" "${3:-}" "${4:-true}" ;;
  gov) gov ;;
  logs) tail -f "$D/node.log" ;;
  *) echo "usage: ./cmd [up|down|reset|status|submit [acct]|propose [acct param period]|vote [acct id approve]|gov|logs]" >&2; exit 2 ;;
esac
