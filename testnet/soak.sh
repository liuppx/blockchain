#!/usr/bin/env bash
# M232: long-run stability SOAK against a RUNNING testnet (gen.sh → up.sh first).
# Closes part of the "无长跑 soak" gap (SECURITY.md §10.5): runs the multi-validator chain
# for a sustained window and asserts, every sample, the invariants that a leak / clock drift /
# state divergence / cert-log bloat would break:
#
#   - LIVENESS        every node's height strictly INCREASES between samples (no stall)
#   - NO FORK/DRIFT   all nodes report the SAME block hash AND state_root at a trailing height
#   - SUPPLY          /supply conserved=true on every node, and the supply figure is IDENTICAL
#                     across nodes (shared consensus state, not just locally consistent)
#   - NO LEAK         each node's RSS grows < LEAK_PCT% over the whole run
#   - CLEAN LOGS      no error/panic lines appear in any node log during the run
#
# Load-free: relies on the create_empty_blocks heartbeat (testnet default) so thousands of
# blocks flow with no tx authoring. A tx-LOAD soak additionally needs raw-seed accounts
# (testnet ships encrypted keystores, which `encode-tx --key-file` can't consume) — tracked
# as a follow-up. Real pre-mainnet soak = hours/days; this is a repeatable minutes-scale gate.
#
#   testnet/soak.sh [DURATION_S=180] [INTERVAL_S=10] [LAG=2] [LEAK_PCT=50]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"
DURATION="${1:-180}"
INTERVAL="${2:-10}"
LAG="${3:-2}"          # trailing height offset for agreement checks (avoid tip races)
LEAK_PCT="${4:-50}"    # max tolerated RSS growth over the run, percent

ids=()
for d in "$OUT"/n*/; do ids+=("$(basename "$d" | sed 's/^n//')"); done
IFS=$'\n' ids=($(sort -n <<<"${ids[*]}")); unset IFS
[ ${#ids[@]} -gt 0 ] || { echo "no nodes generated (run gen.sh + up.sh)" >&2; exit 1; }

getj()    { curl -sf --max-time 2 -H "Accept: application/json" "http://127.0.0.1:92$1$2" 2>/dev/null; }
height()  { getj "$1" /height | grep -oE '[0-9]+' | head -1; }
# precise JSON scalar extractors (quoted string / bare bool); first match
jstr()    { grep -oE "\"$1\":\"[0-9a-f]+\"" | head -1 | cut -d'"' -f4; }
jbool()   { grep -oE "\"$1\":(true|false)" | head -1 | cut -d: -f2; }
pid_of()  { cat "$OUT/n$1/pid" 2>/dev/null; }
rss_of()  { ps -o rss= -p "$(pid_of "$1")" 2>/dev/null | tr -d ' '; }
errs_of() { grep -icE 'error|panic|FATAL' "$OUT/n$1/node.log" 2>/dev/null || true; }

# baseline: all nodes up (bash 3.2: parallel indexed arrays, index i ↔ ids[i])
prevh=(); rss0=(); err0=()
for i in "${!ids[@]}"; do
  id="${ids[$i]}"
  p="$(pid_of "$id")"; [ -n "$p" ] && kill -0 "$p" 2>/dev/null || { echo "n$id not running — run testnet/up.sh" >&2; exit 1; }
  prevh[$i]="$(height "$id")"; rss0[$i]="$(rss_of "$id")"; err0[$i]="$(errs_of "$id")"
done
echo "== soak: ${#ids[@]} nodes [${ids[*]}] for ${DURATION}s, sample every ${INTERVAL}s (lag $LAG, leak<${LEAK_PCT}%)"

samples=0
deadline=$((SECONDS + DURATION))
while [ $SECONDS -lt $deadline ]; do
  sleep "$INTERVAL"
  samples=$((samples + 1))

  # collect heights; assert strict increase (liveness)
  minh=999999999
  for i in "${!ids[@]}"; do
    id="${ids[$i]}"
    h="$(height "$id")"; case "$h" in ''|*[!0-9]*) echo "FAIL: n$id height unreadable" >&2; exit 1 ;; esac
    [ "$h" -gt "${prevh[$i]}" ] || { echo "FAIL: n$id did not advance (${prevh[$i]} → $h) — stall" >&2; exit 1; }
    prevh[$i]="$h"; [ "$h" -lt "$minh" ] && minh="$h"
  done

  # agreement at a trailing height: block hash + state_root + supply identical across nodes
  ch=$((minh - LAG)); [ "$ch" -lt 1 ] && ch=1
  rhash=""; rroot=""; rsup=""
  for id in "${ids[@]}"; do
    blk="$(getj "$id" "/block/$ch")"
    hash="$(printf '%s' "$blk" | jstr hash)"
    root="$(printf '%s' "$blk" | jstr state_root)"
    sj="$(getj "$id" /supply)"
    sup="$(printf '%s' "$sj" | jstr supply)"
    con="$(printf '%s' "$sj" | jbool conserved)"
    [ "$con" = "true" ] || { echo "FAIL: n$id supply NOT conserved at h$minh" >&2; exit 1; }
    [ -n "$hash" ] && [ -n "$root" ] && [ -n "$sup" ] || { echo "FAIL: n$id missing hash/root/supply at $ch" >&2; exit 1; }
    if [ -z "$rhash" ]; then rhash="$hash"; rroot="$root"; rsup="$sup"
    else
      [ "$hash" = "$rhash" ] || { echo "FAIL: FORK n$id block $ch hash $hash != $rhash" >&2; exit 1; }
      [ "$root" = "$rroot" ] || { echo "FAIL: DRIFT n$id state_root@$ch $root != $rroot" >&2; exit 1; }
      [ "$sup"  = "$rsup"  ] || { echo "FAIL: SUPPLY n$id $sup != $rsup" >&2; exit 1; }
    fi
  done
  echo "  sample $samples: min height $minh | agree @$ch root=${rroot:0:10}… supply=$rsup conserved ✓"
done

# post-run: leak guard + clean-log guard
echo "== post-soak checks after $samples samples (~${DURATION}s)"
fail=0
for i in "${!ids[@]}"; do
  id="${ids[$i]}"
  r1="$(rss_of "$id")"; r0="${rss0[$i]}"
  grow=$(( r0 > 0 ? (r1 - r0) * 100 / r0 : 0 ))
  e1="$(errs_of "$id")"; enew=$(( e1 - ${err0[$i]} ))
  printf "  n%s  RSS %sKB → %sKB (%+d%%)  new-log-errors %s\n" "$id" "$r0" "$r1" "$grow" "$enew"
  [ "$grow" -le "$LEAK_PCT" ] || { echo "    FAIL: n$id RSS grew ${grow}% > ${LEAK_PCT}% (possible leak)" >&2; fail=1; }
  [ "$enew" -eq 0 ] || { echo "    FAIL: n$id logged $enew new error/panic line(s)" >&2; fail=1; }
done
[ "$fail" = 0 ] || { echo "SOAK FAILED" >&2; exit 1; }
echo "OK: soak passed — liveness, no fork/drift, supply conserved+identical, no leak, clean logs"
