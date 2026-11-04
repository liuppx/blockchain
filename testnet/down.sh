#!/usr/bin/env bash
# M210: gracefully stop every running testnet daemon (SIGTERM, wait for exit).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"

for d in "$OUT"/n*/; do
  d="${d%/}"
  id="$(basename "$d" | sed 's/^n//')"
  # Stop the node first, then its remote signer (M219), if any.
  for role in pid signer.pid; do
    [ -f "$d/$role" ] || continue
    pid="$(cat "$d/$role")"; what="n$id"; [ "$role" = "signer.pid" ] && what="n$id signer"
    if kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid"
      for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
      if kill -0 "$pid" 2>/dev/null; then echo "  $what (pid $pid) did not exit in 10s" >&2
      else echo "  $what stopped"; fi
    else
      echo "  $what not running"
    fi
    rm -f "$d/$role"
  done
done
