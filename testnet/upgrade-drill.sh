#!/usr/bin/env bash
# 升级激活演练(rolling-upgrade S3/S4 的最小形态,node/src/upgrade.rs):
# 在本机起一个 4 验证人测试网,排一次按高度激活的升级,证明三件事:
#
#   1. 时刻表一致的节点越过 H* 后继续出块,且在 H-1、H、H+k 上区块哈希全网相同(一条链);
#   2. 一个没排这次升级的节点(模拟"没升级的验证人")在 H* 当块停下,而不是带着旧规则分叉;
#      剩下 3/4 仍是法定数,链不停;
#   3. 给它补上时刻表、重启,它重放自己的日志并追上,与其他节点在同一条链上。
#
#   testnet/upgrade-drill.sh [H=20]
#
# 全部产物在临时目录(TESTNET_OUT),不碰 testnet/out。结束时停掉全部节点并删除临时目录
# (KEEP=1 保留,便于查看日志)。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
H="${1:-20}"
UPGRADE="knowledge-economy-v1"
DRILL_DIR="$(mktemp -d -t zx-upgrade-drill)"
export TESTNET_OUT="$DRILL_DIR/net"   # gen.sh 要求目录尚不存在(防止误删已有的 keystore)
OUT="$TESTNET_OUT"
LAG=24   # 没升级的节点

cleanup() {
  "$HERE/down.sh" >/dev/null 2>&1 || true
  if [ "${KEEP:-0}" = 1 ]; then echo "(保留演练目录 $OUT)"; else rm -rf "$DRILL_DIR"; fi
}
trap cleanup EXIT

[ -x "$BIN" ] || { echo "先构建:cargo build --release --manifest-path node/Cargo.toml" >&2; exit 1; }
for id in 21 22 23 24; do
  if command -v lsof >/dev/null && lsof -nP -iTCP:"90$id" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "端口 90$id 被占用:本机已有测试网在跑?先 testnet/down.sh" >&2; exit 1
  fi
done

# 节点刚启动、还没监听时 curl 会失败;这里一律吞掉错误(set -e + pipefail 下否则整个脚本退出)
get() { curl -sf --max-time 2 "http://127.0.0.1:92$1$2" || true; }
height() { get "$1" /height | tr -dc 0-9 || true; }
field() { get "$1" "/block/$2" | awk -F= -v k="$3" '$1==k{print $2}' || true; }
wait_height() { # id min timeout
  local deadline=$((SECONDS + $3)) h
  while :; do
    h="$(height "$1")"; [ -n "$h" ] && [ "$h" -ge "$2" ] && return 0
    [ $SECONDS -ge $deadline ] && { echo "n$1 没有在 $3 秒内到达高度 $2(当前 ${h:-?})" >&2; return 1; }
    sleep 1
  done
}
same_block() { # height ids...
  local hgt="$1"; shift; local ref="" id b
  for id in "$@"; do
    b="$(field "$id" "$hgt" hash)"
    [ -n "$b" ] || { echo "n$id 取不到高度 $hgt 的区块" >&2; return 1; }
    if [ -z "$ref" ]; then ref="$b"; elif [ "$b" != "$ref" ]; then
      echo "分叉:高度 $hgt n$id=$b 与 $ref 不同" >&2; return 1
    fi
  done
  echo "  高度 $hgt:$* 区块哈希相同 ${ref:0:16}…"
}

echo "== 生成测试网(升级 $UPGRADE 于高度 $H),目录 $OUT"
"$HERE/gen.sh" --drill --upgrade "$UPGRADE=$H" >/dev/null
grep -q "^upgrades $UPGRADE@$H\$" "$OUT/genesis-identity.txt" \
  || { echo "genesis-hash 没有打印时刻表" >&2; cat "$OUT/genesis-identity.txt"; exit 1; }
echo "  链身份:$(head -1 "$OUT/genesis-identity.txt")"
echo "  时刻表:$(tail -1 "$OUT/genesis-identity.txt")"

# n24 用一份没有这次升级的创世(模拟没升级的验证人)。创世哈希相同——时刻表不进创世哈希——所以它照常入网。
awk '/^\[\[upgrades\]\]/{skip=3} skip>0{skip--; next} {print}' "$OUT/genesis.toml" > "$OUT/genesis-lagging.toml"
sed -i.bak "s|^genesis = .*|genesis = \"$OUT/genesis-lagging.toml\"|" "$OUT/n$LAG/node.toml"
lag_id="$("$BIN" genesis-hash --config "$OUT/n$LAG/node.toml")"
[ "$(echo "$lag_id" | head -1)" = "$(head -1 "$OUT/genesis-identity.txt")" ] \
  || { echo "没升级的节点创世哈希应当相同" >&2; exit 1; }
echo "  n$LAG:$(echo "$lag_id" | tail -1)(创世哈希相同)"

echo "== 启动 4 个验证人"
"$HERE/up.sh" >/dev/null

echo "== 1) H* 之前:4 个节点同一条链"
for id in 21 22 23 24; do wait_height "$id" $((H - 1)) 120; done
same_block $((H - 1)) 21 22 23 24

echo "== 2) 越过 H*:升级的 3 个节点继续出块,n$LAG 停在 H-1"
for id in 21 22 23; do wait_height "$id" $((H + 8)) 120; done
same_block "$H" 21 22 23
same_block $((H + 8)) 21 22 23
sleep 5
lag_h="$(height "$LAG")"
echo "  n$LAG 高度 $lag_h(升级节点已到 $(height 21))"
[ "$lag_h" = $((H - 1)) ] || { echo "没升级的节点应停在 $((H - 1)),实际 $lag_h" >&2; exit 1; }
grep -qiE "StateRootMismatch|state.?root" "$OUT/n$LAG/node.log" \
  && echo "  n$LAG 日志:在 H* 报状态根不一致(停下,没有分叉)" \
  || echo "  (n$LAG 日志里没找到 StateRootMismatch 字样;高度已证明它停在 H-1)"

echo "== 3) 给 n$LAG 补上时刻表并重启:重放日志后追上"
kill -TERM "$(cat "$OUT/n$LAG/pid")"; sleep 3
mv "$OUT/n$LAG/node.toml.bak" "$OUT/n$LAG/node.toml"
"$HERE/up.sh" >/dev/null
target=$(( $(height 21) + 3 ))
wait_height "$LAG" "$target" 120
same_block "$H" 21 22 23 24
same_block "$target" 21 22 23 24

echo "OK:H* 前逐字节一致;时刻表不一致的节点在 H* 停下而不分叉;补上时刻表后追上同一条链"
