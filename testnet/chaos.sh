#!/usr/bin/env bash
# M231: chaos / fault-injection drill against a RUNNING testnet (gen.sh → up.sh first).
# Closes the "无混沌测" gap (SECURITY.md §10.5 / production-readiness #5) with the two
# fault scenarios reachable on loopback without root/iptables:
#
#   Phase A  crash tolerance + recovery — SIGKILL a MINORITY of validators (the BFT
#            fault budget f = N − quorum). The surviving >2/3 must KEEP committing and
#            stay on ONE chain (liveness + no fork while f are down). Restart the
#            victims; each must CATCH BACK UP to the common head (crash-recovery replay).
#   Phase B  safety under quorum loss — SIGKILL one MORE (now < quorum online). The chain
#            must HALT (no block commits without >2/3) — proving it stops rather than
#            FORKS when quorum is lost. Restart ONLY the crashed nodes; the chain must
#            AUTO-RESUME on the same chain without touching the survivors (M238 §10.10:
#            on (re)connect a node re-delivers its in-flight-height votes so a laggard
#            skips up to the live round — before M238 this needed a full-set restart).
#
# SIGKILL (not SIGTERM) is deliberate: store.rs fsyncs every record (sync_all per block),
# so a hard kill leaves clean record boundaries and restart replays cleanly — this tests
# real crash/power-loss recovery, not a graceful drain.
#
#   testnet/chaos.sh [DELTA=3] [TIMEOUT_S=90] [HOLD_S=15]
#     DELTA   heights the chain must advance to prove liveness / resume
#     TIMEOUT per-wait ceiling
#     HOLD    how long to hold the sub-quorum partition and confirm no progress
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/out"
DELTA="${1:-3}"
TIMEOUT="${2:-90}"
HOLD="${3:-15}"

ids=()
for d in "$OUT"/n*/; do ids+=("$(basename "$d" | sed 's/^n//')"); done
IFS=$'\n' ids=($(sort -n <<<"${ids[*]}")); unset IFS
N=${#ids[@]}
[ "$N" -gt 0 ] || { echo "no nodes generated (run gen.sh + up.sh)" >&2; exit 1; }

# BFT quorum over equal-power validators: need v > 2N/3 votes to commit.
QUORUM=$(( 2 * N / 3 + 1 ))   # floor(2N/3)+1
F_LIVE=$(( N - QUORUM ))       # max simultaneous crashes that still keep liveness
echo "== testnet: N=$N validators, quorum=$QUORUM, fault budget f=$F_LIVE (ids: ${ids[*]})"
[ "$F_LIVE" -ge 1 ] || { echo "N=$N tolerates 0 crashes (need N>=4); nothing to drill" >&2; exit 1; }

get()  { curl -sf --max-time 2 "http://127.0.0.1:92$1$2" 2>/dev/null; }
h_of() { local h; h="$(get "$1" /height || echo -)"; case "$h" in ''|*[!0-9]*) echo -1 ;; *) echo "$h" ;; esac; }
hash_at() { get "$1" "/block/$2" | awk -F= '$1=="hash"{print $2}'; }
pid_of()  { cat "$OUT/n$1/pid" 2>/dev/null || echo ""; }
alive()   { local p; p="$(pid_of "$1")"; [ -n "$p" ] && kill -0 "$p" 2>/dev/null; }

kill_node() {  # hard crash (SIGKILL) — does NOT touch a remote signer (separate process)
  local id="$1" p; p="$(pid_of "$id")"
  [ -n "$p" ] && kill -9 "$p" 2>/dev/null || true
  for _ in $(seq 1 50); do alive "$id" || break; sleep 0.1; done
  echo "  !! n$id KILLED (pid $p)"
}

# min / max committed height across a list of ids
min_h() { local m=999999999 id h; for id in "$@"; do h="$(h_of "$id")"; [ "$h" -lt "$m" ] && m="$h"; done; echo "$m"; }
max_h() { local m=-1 id h; for id in "$@"; do h="$(h_of "$id")"; [ "$h" -gt "$m" ] && m="$h"; done; echo "$m"; }

# Wait until min height across the given ids reaches TARGET (liveness). Returns 1 on timeout.
wait_min_height() {
  local target="$1"; shift
  local deadline=$((SECONDS + TIMEOUT))
  while :; do
    local m; m="$(min_h "$@")"
    echo "    min height across [$*] = $m (target $target)"
    [ "$m" -ge "$target" ] && return 0
    [ $SECONDS -ge $deadline ] && return 1
    sleep 2
  done
}

# Assert every id reports the SAME block hash at height H (one chain). Returns 1 on fork.
assert_no_fork() {
  local h="$1"; shift; local ref="" id x
  for id in "$@"; do
    x="$(hash_at "$id" "$h")"
    [ -n "$x" ] || { echo "    n$id: no hash at $h" >&2; return 1; }
    if [ -z "$ref" ]; then ref="$x"; echo "    n$id @$h $x";
    elif [ "$x" != "$ref" ]; then echo "    FORK: n$id @$h $x != $ref" >&2; return 1
    else echo "    n$id @$h $x (same)"; fi
  done
  return 0
}

# require a healthy, converged starting point
for id in "${ids[@]}"; do alive "$id" || { echo "n$id not running — run testnet/up.sh" >&2; exit 1; }; done
start="$(min_h "${ids[@]}")"
echo "== baseline: all $N nodes up, min height $start"

# ---- Phase A: crash the fault budget (minority), expect liveness + recovery -------------
victimsA=("${ids[@]: -$F_LIVE}")                 # last F_LIVE ids
survivors=("${ids[@]:0:$((N - F_LIVE))}")
echo
echo "== Phase A: SIGKILL $F_LIVE validator(s) [${victimsA[*]}] — survivors [${survivors[*]}] must keep committing"
for id in "${victimsA[@]}"; do kill_node "$id"; done
base="$(min_h "${survivors[@]}")"
echo "  survivors at min height $base; expecting advance to $((base + DELTA))"
wait_min_height $((base + DELTA)) "${survivors[@]}" \
  || { echo "FAIL: liveness lost with only $F_LIVE down (should tolerate f=$F_LIVE)" >&2; exit 1; }
chk="$(min_h "${survivors[@]}")"
echo "  checking survivors agree on block $chk (no fork while $F_LIVE down)"
assert_no_fork "$chk" "${survivors[@]}" || { echo "FAIL: survivors forked" >&2; exit 1; }
echo "  PASS: chain advanced + stayed single while $F_LIVE crashed"

echo "  restarting victim(s) [${victimsA[*]}] …"
"$HERE/up.sh" >/dev/null
tip="$(max_h "${ids[@]}")"
echo "  waiting for recovered node(s) to catch up to >= $tip"
wait_min_height "$tip" "${victimsA[@]}" \
  || { echo "FAIL: crashed node did not recover/catch up" >&2; exit 1; }
rec="$(min_h "${ids[@]}")"
assert_no_fork "$rec" "${ids[@]}" || { echo "FAIL: recovered node on a different chain" >&2; exit 1; }
echo "  PASS: crashed validator(s) recovered and rejoined the one chain"

# ---- Phase B: drop below quorum, expect HALT (safety), then resume ----------------------
F_HALT=$((F_LIVE + 1))                            # one past the fault budget → < quorum online
victimsB=("${ids[@]: -$F_HALT}")
onlineB=("${ids[@]:0:$((N - F_HALT))}")
echo
echo "== Phase B: SIGKILL $F_HALT total [${victimsB[*]}] — only ${#onlineB[@]} online (< quorum $QUORUM): must HALT"
for id in "${victimsB[@]}"; do alive "$id" && kill_node "$id"; done
sleep 5                                           # let any in-flight block settle
hs="$(max_h "${onlineB[@]}")"
echo "  height with sub-quorum online: $hs — holding ${HOLD}s to confirm NO new commits"
sleep "$HOLD"
he="$(max_h "${onlineB[@]}")"
echo "  height after hold: $he"
if [ "$he" -gt "$hs" ]; then
  echo "FAIL: chain advanced ($hs → $he) without quorum — safety violation" >&2; exit 1
fi
echo "  PASS: chain HALTED below quorum (no commit without >2/3 ⇒ cannot fork)"

# M238 (§10.10): restart ONLY the crashed nodes and expect AUTOMATIC recovery. Before M238
# this wedged — survivors churned to a high round during the halt, the restarted nodes came
# back at round 0, and fire-and-forget gossip never replayed the survivors' past votes, so the
# L55 round-skip could not fire (a full-set restart was the only cure). M238 re-delivers each
# node's own in-flight-height votes to any (re)connecting peer, so the restarted nodes observe
# f+1 at the live round and skip up — the chain resumes WITHOUT touching the survivors.
echo "  restarting ONLY the crashed node(s) [${victimsB[*]}] — survivors stay up (M238 auto-recovery) …"
"$HERE/up.sh" >/dev/null
echo "  waiting for the chain to auto-resume + advance to >= $((he + DELTA))"
wait_min_height $((he + DELTA)) "${ids[@]}" \
  || { echo "FAIL: chain did not auto-recover after quorum restored (§10.10 regression)" >&2; exit 1; }
fin="$(min_h "${ids[@]}")"
assert_no_fork "$fin" "${ids[@]}" || { echo "FAIL: forked after resume" >&2; exit 1; }
echo "  PASS: quorum restored → chain AUTO-resumed on the SAME chain at height $fin (no full restart)"

echo
echo "OK: chaos drill passed — crash tolerance (f=$F_LIVE), crash recovery, quorum-loss"
echo "    safety-halt, and resume all held on one chain."
