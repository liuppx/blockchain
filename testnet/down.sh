#!/usr/bin/env bash
# M210: gracefully stop every running testnet daemon (SIGTERM, wait for exit).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"

for d in "$OUT"/n*/; do
  d="${d%/}"
  [ -f "$d/pid" ] || continue
  id="$(basename "$d" | sed 's/^n//')"; pid="$(cat "$d/pid")"
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid"
    for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.2; done
    if kill -0 "$pid" 2>/dev/null; then echo "  n$id (pid $pid) did not exit in 10s" >&2
    else echo "  n$id stopped"; fi
  else
    echo "  n$id not running"
  fi
  rm -f "$d/pid"
done
