#!/usr/bin/env bash
# M210: start every generated validator as a background daemon (DEPLOYMENT.md §6).
# Logs → testnet/out/n<id>/node.log, pid → testnet/out/n<id>/pid. Stop with down.sh
# (SIGTERM → graceful shutdown; never SIGKILL a validator).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
OUT="$HERE/out"
[ -f "$OUT/password" ] || { echo "run testnet/gen.sh first" >&2; exit 1; }

export ZHIXING_KEYSTORE_PASSWORD="$(cat "$OUT/password")"
export RUST_LOG="${RUST_LOG:-info}"

for d in "$OUT"/n*/; do
  d="${d%/}"
  id="$(basename "$d" | sed 's/^n//')"
  if [ -f "$d/pid" ] && kill -0 "$(cat "$d/pid")" 2>/dev/null; then
    echo "  n$id already running (pid $(cat "$d/pid"))"; continue
  fi
  nohup "$BIN" run --config "$d/node.toml" >> "$d/node.log" 2>&1 &
  echo $! > "$d/pid"
  echo "  n$id started (pid $!)  log: $d/node.log"
done
echo "metrics: http://127.0.0.1:91<id>/metrics  (Bearer METRICS_TOKEN from out/tokens.env)"
echo "rpc:     http://127.0.0.1:92<id>/height | /head | /header/{h} | /proposals"
