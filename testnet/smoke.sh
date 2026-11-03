#!/usr/bin/env bash
# M210: convergence smoke test against a running testnet — every node must reach
# MIN_HEIGHT within TIMEOUT seconds and report the SAME cert-signed header at that
# height (one chain, not N). Uses the public RPC read face (GET needs no token).
#
#   testnet/smoke.sh [MIN_HEIGHT=3] [TIMEOUT_S=90]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"
MIN="${1:-3}"
TIMEOUT="${2:-90}"

ids=()
for d in "$OUT"/n*/; do ids+=("$(basename "$d" | sed 's/^n//')"); done
[ ${#ids[@]} -gt 0 ] || { echo "no nodes generated (run gen.sh)" >&2; exit 1; }

get() { curl -sf --max-time 2 "http://127.0.0.1:92$1$2"; }

echo "== waiting for every node to reach height >= $MIN (timeout ${TIMEOUT}s)"
deadline=$((SECONDS + TIMEOUT))
while :; do
  all=1; line=""
  for id in "${ids[@]}"; do
    h="$(get "$id" /height 2>/dev/null || echo "-")"
    line="$line n$id=$h"
    case "$h" in ''|*[!0-9]*) all=0 ;; *) [ "$h" -ge "$MIN" ] || all=0 ;; esac
  done
  echo "  $line"
  [ "$all" = 1 ] && break
  if [ $SECONDS -ge $deadline ]; then echo "TIMEOUT: nodes did not all reach $MIN" >&2; exit 1; fi
  sleep 2
done

echo "== comparing the committed block hash at height $MIN across nodes"
# NOT /header/{h}: that is a *certified* header, and each node assembles its own
# commit certificate (any >2/3 signer subset), so the cert bytes may differ per node
# while the chain is one. The block hash is the node-independent witness.
ref=""
for id in "${ids[@]}"; do
  hash="$(get "$id" "/block/$MIN" | awk -F= '$1=="hash"{print $2}')"
  [ -n "$hash" ] || { echo "n$id: no hash in /block/$MIN response" >&2; exit 1; }
  if [ -z "$ref" ]; then ref="$hash"; echo "  n$id $hash";
  elif [ "$hash" != "$ref" ]; then echo "FORK: n$id block $MIN hash $hash != $ref" >&2; exit 1
  else echo "  n$id $hash (same)"; fi
done
echo "OK: ${#ids[@]} nodes converged on one chain at height $MIN"
