#!/usr/bin/env bash
# M210: generate a local multi-validator testnet — encrypted keystores, one shared
# genesis.toml (M200 calibration values), and a hardened per-node config — then
# run `check-config` + `genesis-hash` on every node and require the identity to
# match. This scripts exactly the manual steps of docs/DEPLOYMENT.md §2–§4 so the
# §11 checklist item "genesis 参数经测试网实测反校" has a reproducible starting point.
#
#   testnet/gen.sh [-n VALIDATORS] [-a ACCOUNTS] [--force]
#
# Everything lands in testnet/out/ (gitignored: it holds keystores + the password).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
OUT="$HERE/out"

N=4          # validators (ids 21..)  — ≥4 tolerates 1 Byzantine (3f+1, f=1)
ACCOUNTS=3   # funded accounts (ids 1..) — tx authors for load / calibration
FORCE=0
SIGNER_IDS=""   # M219: comma-separated validator ids to run behind a remote `node signer`
PERMISSIONED=0  # M236: --permissioned ⇒ genesis-pinned mTLS + RFC5705 channel binding
while [ $# -gt 0 ]; do
  case "$1" in
    -n) N="$2"; shift 2 ;;
    -a) ACCOUNTS="$2"; shift 2 ;;
    --signer) SIGNER_IDS="$2"; shift 2 ;;
    --permissioned) PERMISSIONED=1; shift ;;
    --force) FORCE=1; shift ;;
    *) echo "usage: $0 [-n VALIDATORS] [-a ACCOUNTS] [--signer id1,id2,...] [--permissioned] [--force]" >&2; exit 2 ;;
  esac
done
if [ "$N" -lt 4 ]; then echo "need at least 4 validators (BFT 3f+1)" >&2; exit 2; fi

# M219: is validator `$1` configured to sign via a remote `node signer`?
is_signer_id() {
  case ",$SIGNER_IDS," in *",$1,"*) return 0 ;; *) return 1 ;; esac
}

if [ -e "$OUT" ]; then
  if [ "$FORCE" = 1 ]; then rm -rf "$OUT"; else
    echo "$OUT exists — pass --force to regenerate (this DESTROYS the keystores)" >&2; exit 1
  fi
fi
mkdir -p "$OUT/keys"
chmod 700 "$OUT" "$OUT/keys"

if [ ! -x "$BIN" ]; then
  echo "building release binary…"
  (cd "$ROOT/node" && cargo build --release)
fi

rand_hex() { # 32 hex chars, portable (openssl → /dev/urandom fallback)
  if command -v openssl >/dev/null 2>&1; then openssl rand -hex 16
  else head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'; fi
}

# One keystore password for the whole testnet (production: one per validator,
# injected via systemd EnvironmentFile). Honoured by keygen --encrypt and node run.
PW="${ZHIXING_KEYSTORE_PASSWORD:-$(rand_hex)}"
printf '%s' "$PW" > "$OUT/password"; chmod 600 "$OUT/password"
export ZHIXING_KEYSTORE_PASSWORD="$PW"

METRICS_TOKEN="$(rand_hex)"
RPC_TOKEN="$(rand_hex)"
{ echo "METRICS_TOKEN=$METRICS_TOKEN"; echo "RPC_TOKEN=$RPC_TOKEN"; } > "$OUT/tokens.env"
chmod 600 "$OUT/tokens.env"

# keygen --encrypt prints "pubkey <hex>" — harvest it, never touch the keystore.
mint_key() { # $1 = keystore path  → echoes pubkey hex
  "$BIN" keygen --encrypt --out "$1" | awk '$1=="pubkey"{print $2}'
}

GEN="$OUT/genesis.toml"
{
  echo "# zhixing testnet genesis — shared by every node, byte-identical."
  echo "# Params: M200 calibration (sim/CALIBRATION.md). Tune via on-chain governance later."
  echo "base_emission_micro    = 8000000"
  echo "decay_half_life_blocks = 0"
  echo "slash_bps              = 10000"
  echo "max_validator_power_bps = 3333   # M235 §10.11: no new validator may exceed 1/3 of power"
  echo "timestamp_days         = 0.0"
  echo
  for ((i = 1; i <= ACCOUNTS; i++)); do
    pk="$(mint_key "$OUT/keys/acct$i.json")"
    echo "[[accounts]]"; echo "id = $i"; echo "balance_micro = 30000000"; echo "pubkey_hex = \"$pk\""; echo
  done
  for ((i = 1; i <= ACCOUNTS; i++)); do
    echo "[[reviewers]]"; echo "id = $((9 + i))"; echo "weight = 1.0"; echo
  done
  for ((i = 1; i <= N; i++)); do
    id=$((20 + i))
    pk="$(mint_key "$OUT/keys/val$id.json")"
    echo "[[validators]]"; echo "id = $id"; echo "pubkey_hex = \"$pk\""; echo "power = 1"; echo
  done
} > "$GEN"
chmod 600 "$OUT"/keys/*.json

# Per-node config (DEPLOYMENT.md §4/§5). Ports: p2p 90<id>, metrics 91<id>, rpc 92<id>.
for ((i = 1; i <= N; i++)); do
  id=$((20 + i)); d="$OUT/n$id"; mkdir -p "$d/data"
  {
    echo "genesis = \"$GEN\""
    echo
    echo "[node]"; echo "id = $id"; echo "listen = \"127.0.0.1:90$id\""; echo "data_dir = \"$d/data\""
    for ((j = 1; j <= N; j++)); do
      pid=$((20 + j)); [ "$pid" = "$id" ] && continue
      echo; echo "[[peers]]"; echo "id = $pid"; echo "addr = \"127.0.0.1:90$pid\""
    done
    echo
    echo "[validator]"; echo "enabled = true"
    if is_signer_id "$id"; then
      # M219: key lives in an out-of-process `node signer`; this node holds none on disk.
      # up.sh starts the signer (keys/val<id>.json) on this socket before the node.
      sock="/tmp/zxsig-$id-$RANDOM.sock"
      echo "$sock" > "$d/signer.socket"
      echo "$id" >> "$OUT/signers.txt"
      echo "[validator.signer]"; echo "socket = \"$sock\""; echo "timeout_ms = 1000"
    else
      echo "encrypted_keystore_path = \"$OUT/keys/val$id.json\""
    fi
    echo
    echo "[consensus]"; echo "block_interval_ms = 1000"
    echo "# Heartbeat blocks so convergence is observable with no load; production: false."
    echo "create_empty_blocks = true"
    echo
    echo "[network]  # DEPLOYMENT.md §5 hardening on, as in production"
    echo "enable_tls = true"; echo "require_peer_auth = true"; echo "max_inbound_connections = 64"
    if [ "$PERMISSIONED" = 1 ]; then
      # M236: fully-hardened permissioned posture — genesis-pinned mutual TLS (RFC7250
      # raw-pubkey, keyed by each node's genesis ed25519 seed; no cert files) + RFC5705
      # channel binding (anti-relay; needs TLS + peer-auth).
      echo "require_peer_certs = true"; echo "bind_channel = true"
    else
      echo "# require_peer_certs = true   # permissioned nets: genesis-pinned mTLS (--permissioned)"
      echo "# bind_channel = true         # RFC5705 channel binding (needs TLS + auth)"
    fi
    echo
    echo "[metrics]"; echo "enabled = true"; echo "listen = \"127.0.0.1:91$id\""
    echo "auth_token = \"$METRICS_TOKEN\""; echo "enable_tls = false  # loopback only"
    echo
    echo "[rpc]"; echo "enabled = true"; echo "listen = \"127.0.0.1:92$id\""
    echo "auth_token = \"$RPC_TOKEN\""; echo "enable_tls = false  # loopback only; non-loopback MUST set true"
  } > "$d/node.toml"
done

# Validate every config + genesis (fail-fast) and require one chain identity.
echo "== check-config"
first=""
for ((i = 1; i <= N; i++)); do
  id=$((20 + i)); cfg="$OUT/n$id/node.toml"
  "$BIN" check-config --config "$cfg" >/dev/null
  idn="$("$BIN" genesis-hash --config "$cfg")"
  if [ -z "$first" ]; then first="$idn"; elif [ "$idn" != "$first" ]; then
    echo "genesis identity differs on node $id" >&2; exit 1
  fi
  echo "  n$id ok"
done
echo "== chain identity (compare out-of-band across operators)"
echo "$first" | sed 's/^/  /'
echo "$first" > "$OUT/genesis-identity.txt"
echo
echo "generated: $OUT  (keystores + password inside — NOT for git)"
echo "next: testnet/up.sh && testnet/smoke.sh"
