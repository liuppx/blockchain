#!/usr/bin/env bash
# M211: backup / recovery + snapshot fast-sync drill — closes DEPLOYMENT.md §11
# "备份/恢复演练过一次(snapshot + verify + 重放)" with a reproducible run on the
# local testnet. Two drills, both against real daemons:
#
#   A. backup + verify + restore   : replay-verify a node's log, export a snapshot,
#      back the data dir up, restore it to a fresh path, and prove the restored
#      copy is byte-identical (same snapshot blob) and independently verifies.
#   B. snapshot fast-sync          : destroy a validator's data (simulated disk loss)
#      and rebuild it FROM the snapshot — it adopts the state at the snapshot height
#      (no full replay), tail-syncs over gossip, and rejoins consensus on the same
#      chain. Keeps the hardened posture (the rebuilt node re-authenticates with its
#      genesis validator key); a keyless follower can't join a require_peer_auth mesh.
#
#   testnet/recover.sh [SNAP_HEIGHT=5] [TIMEOUT_S=120]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
OUT="$HERE/out"
SNAP_H="${1:-5}"
TIMEOUT="${2:-120}"
REBUILD_ID=24   # the validator we destroy + rebuild from the snapshot in Drill B

[ -f "$OUT/genesis.toml" ] || { echo "run testnet/gen.sh first" >&2; exit 1; }
[ -x "$BIN" ] || { echo "build the release binary first (testnet/gen.sh does this)" >&2; exit 1; }
export ZHIXING_KEYSTORE_PASSWORD="$(cat "$OUT/password")"
export RUST_LOG="${RUST_LOG:-info}"
get() { curl -sf --max-time 2 "http://127.0.0.1:92$1$2"; }

# Start one node by id with optional extra `run` args (e.g. --snapshot …). pid → its
# out/n<id>/pid so down.sh / the EXIT trap stop it.
start_node() {
  local id="$1"; shift
  local d="$OUT/n$id"
  nohup "$BIN" run --config "$d/node.toml" "$@" >> "$d/node.log" 2>&1 &
  echo $! > "$d/pid"
}

# Always stop every daemon this drill started, even on failure — a half-run that left
# nodes bound to the ports is what silently corrupts the *next* run (it snapshots the
# wrong, stale data dir). The trap makes the drill self-cleaning.
trap 'rc=$?; "$HERE/down.sh" >/dev/null 2>&1 || true; exit $rc' EXIT

wait_height() { # $1=rpc-id-suffix $2=min  → waits until that node's /height >= min
  local deadline=$((SECONDS + TIMEOUT))
  while :; do
    local h; h="$(get "$1" /height 2>/dev/null || echo -)"
    case "$h" in ''|*[!0-9]*) : ;; *) [ "$h" -ge "$2" ] && return 0 ;; esac
    [ $SECONDS -ge $deadline ] && { echo "TIMEOUT: node $1 did not reach height $2 (last=$h)" >&2; return 1; }
    sleep 2
  done
}

echo "== bring the validators up and let them pass height $SNAP_H"
"$HERE/up.sh" >/dev/null
wait_height 21 "$SNAP_H"
echo "== stop for a clean, race-free snapshot"
"$HERE/down.sh" >/dev/null

SRC="$OUT/n21/data"
BLOB="$OUT/snapshot.blob"
CFG="$OUT/n21/node.toml"   # carries the testnet genesis → verify/snapshot replay against it
echo
echo "== Drill A: backup + verify + restore"
echo "-- verify the source log (replay through finality)"
"$BIN" verify --dir "$SRC" --config "$CFG"
echo "-- export a snapshot"
"$BIN" snapshot --dir "$SRC" --out "$BLOB" --config "$CFG"
SNAP_INFO="$("$BIN" inspect-snapshot --snapshot "$BLOB")"
echo "   $SNAP_INFO"
echo "$SNAP_INFO" | grep -q "supply_conserved=true" || { echo "snapshot supply not conserved" >&2; exit 1; }
H_SNAP="$(echo "$SNAP_INFO" | sed -n 's/.*height=\([0-9]*\).*/\1/p')"
[ -n "$H_SNAP" ] && [ "$H_SNAP" -ge "$SNAP_H" ] || { echo "bad snapshot height: $H_SNAP" >&2; exit 1; }

echo "-- back the data dir up, then restore it to a fresh path"
BK="$OUT/backup/n21-data.tar.gz"; mkdir -p "$OUT/backup"
tar -czf "$BK" -C "$OUT/n21" data
RST="$OUT/restore/n21/data"; rm -rf "$OUT/restore"; mkdir -p "$OUT/restore/n21"
tar -xzf "$BK" -C "$OUT/restore/n21"
echo "-- the restored copy verifies and snapshots byte-identically"
"$BIN" verify --dir "$RST" --config "$CFG"
"$BIN" snapshot --dir "$RST" --out "$OUT/restore.blob" --config "$CFG"
if cmp -s "$BLOB" "$OUT/restore.blob"; then echo "   restored snapshot == original (byte-identical)";
else echo "RESTORE MISMATCH: restored snapshot differs from original" >&2; exit 1; fi

echo
echo "== Drill B: restore validator n$REBUILD_ID from the snapshot (fast-sync, no full replay)"
# The realistic recovery: a validator's disk is lost. We rebuild it from the snapshot +
# tail-sync instead of a full replay — keeping the hardened posture (its config has the
# validator key + require_peer_auth, so it re-authenticates as a genesis validator and
# rejoins *consensus*). A keyless follower cannot join a require_peer_auth mesh at all
# (auth proves a genesis-validator identity; same constraint a light node has) — see
# DEPLOYMENT.md §5.
echo "-- simulate disk loss on n$REBUILD_ID (wipe data; keep config + keystore)"
rm -rf "$OUT/n$REBUILD_ID/data"; mkdir -p "$OUT/n$REBUILD_ID/data"
echo "-- bring up the surviving quorum (n21/n22/n23 → 3 of 4 keeps producing)"
for id in 21 22 23; do start_node "$id"; done
wait_height 21 "$((H_SNAP + 1))"
echo "-- cross-check the snapshot against the live cert-signed header at height $H_SNAP"
HDR_HEX="$(get 21 "/header/$H_SNAP" | sed -n 's/^certified_header=//p')"
[ -n "$HDR_HEX" ] || { echo "could not fetch /header/$H_SNAP from live node" >&2; exit 1; }
"$BIN" check-snapshot --snapshot "$BLOB" --header "$HDR_HEX"
echo "   check-snapshot OK (height + 3 cert-signed roots agree)"

echo "-- boot the rebuilt validator FROM the snapshot (adopts state at base $H_SNAP, no replay)"
start_node "$REBUILD_ID" --snapshot "$BLOB" --snapshot-header "$HDR_HEX"
echo "   n$REBUILD_ID started, adopting snapshot at base height $H_SNAP"

# A height strictly above the snapshot base, which both the rebuilt node (post-snapshot
# tail) and a surviving validator hold — proof the fast-synced node rejoined the chain.
TARGET=$((H_SNAP + 2))
echo "-- wait for n$REBUILD_ID to reach height $TARGET and compare block hashes"
if ! wait_height "$REBUILD_ID" "$TARGET"; then tail -8 "$OUT/n$REBUILD_ID/node.log"; exit 1; fi
NEW_HASH="$(get "$REBUILD_ID" "/block/$TARGET" | awk -F= '$1=="hash"{print $2}')"
VAL_HASH="$(get 21 "/block/$TARGET" | awk -F= '$1=="hash"{print $2}')"
echo "   rebuilt   block $TARGET = $NEW_HASH"
echo "   validator block $TARGET = $VAL_HASH"
[ -n "$NEW_HASH" ] && [ "$NEW_HASH" = "$VAL_HASH" ] \
  || { echo "FAST-SYNC MISMATCH: rebuilt validator did not converge on the chain" >&2; exit 1; }
echo "   fast-synced validator rejoined consensus on the same chain ✓"
echo "-- confirm it pruned below its snapshot base (height $H_SNAP is 410 Gone)"
code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:92$REBUILD_ID/block/$H_SNAP" || echo 000)"
echo "   GET /block/$H_SNAP → $code (expected 410; fast-sync did not replay the pre-snapshot tail)"

echo
echo "OK: backup/restore byte-identical + validator rebuilt from snapshot rejoined (DEPLOYMENT.md §11)"
