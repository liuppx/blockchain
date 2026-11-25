#!/usr/bin/env bash
# 新增领域演练(node/src/upgrade.rs 的 domain 部分):
# 在本机起一个 4 验证人测试网,排一次"按高度新增领域",证明三件事:
#
#   1. **加领域不用改代码**:创世里写一行 `[[domains]] id = N height = H` 就够,
#      创世哈希不变(链的身份不变),`genesis-hash` 单独打印时刻表;
#   2. H* 之前用这个领域建单元会被拒,`/domains` 的 next 里也没有它;H* 之后可以;
#   3. 时刻表不一致的节点(模拟"没换创世的验证人")在 H* 当块停下,而不是带着旧规则分叉;
#      剩下 3/4 仍是法定数,链不停;给它补上时刻表、重启,它追上同一条链。
#
#   testnet/domain-drill.sh [H=20] [DOMAIN_ID=3]
#
# 全部产物在临时目录(TESTNET_OUT),不碰 testnet/out。结束时停掉全部节点并删除临时目录
# (KEEP=1 保留,便于查看日志)。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
H="${1:-20}"
DOMAIN_ID="${2:-3}"
DRILL_DIR="$(mktemp -d -t zx-domain-drill)"
export TESTNET_OUT="$DRILL_DIR/net"   # gen.sh 要求目录尚不存在(防止误删已有的 keystore)
OUT="$TESTNET_OUT"
LAG=24   # 没排这次新增领域的节点
RPC=http://127.0.0.1:9221

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
domains() { # id
  curl -sf --max-time 2 -H 'Accept: application/json' "http://127.0.0.1:92$1/domains" || true
}
nonce() { get 21 "/account/$1" | tr ' ' '\n' | awk -F= '$1=="nonce"{print $2}' || true; }
wait_nonce() { # account want timeout
  local d=$((SECONDS + $3))
  while :; do
    [ "$(nonce "$1")" = "$2" ] && return 0
    [ $SECONDS -ge $d ] && { echo "账户 $1 的 op 没入块(当前 nonce $(nonce "$1"))" >&2; return 1; }
    sleep 1
  done
}
# 提交一条"用新领域建单元"的知识 op。返回 HTTP 响应正文(成功是 op 哈希,失败是错误说明)。
create_in_domain() { # actor domain
  local actor="$1" domain="$2"
  local f="$DRILL_DIR/op-$RANDOM.bin" n resp
  n="$(nonce "$actor")"
  # 正文哈希:演练不需要真实正文,链上只存哈希。这里用一个固定哈希。
  "$BIN" encode-knowledge-op --key-file "$OUT/keys/acct$actor.json" --out "$f" \
    --actor "$actor" --nonce "$n" \
    --action create --layer foundation --domain "$domain" \
    --body "zh:$(printf 'domain-drill' | shasum -a 256 | cut -d' ' -f1)" \
    --source "model:drill" >/dev/null
  resp="$(curl -s -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/octet-stream" --data-binary "@$f" "$RPC/knowledge")"
  echo "$resp"
}

echo "== 生成测试网(领域 $DOMAIN_ID 于高度 $H),目录 $OUT"
"$HERE/gen.sh" --drill --domain "$DOMAIN_ID=$H" >/dev/null
grep -q "^upgrades domain-$DOMAIN_ID@$H\$" "$OUT/genesis-identity.txt" \
  || { echo "genesis-hash 没有打印领域时刻表" >&2; cat "$OUT/genesis-identity.txt"; exit 1; }
echo "  链身份:$(head -1 "$OUT/genesis-identity.txt")"
echo "  时刻表:$(tail -1 "$OUT/genesis-identity.txt")"
TOKEN="$(sed -n '/^\[rpc\]/,/^\[/p' "$OUT/n21/node.toml" | grep -m1 auth_token | sed -E 's/.*"([^"]*)".*/\1/')"
export ZHIXING_KEYSTORE_PASSWORD="$(cat "$OUT/password")"

# 记下"有领域排期"时的创世哈希:加 `[[domains]]` 不改链的身份。
BASE_HASH="$(head -1 "$OUT/genesis-identity.txt")"

# n24 用一份没有这次新增领域的创世(模拟没换创世的验证人)。创世哈希相同——时刻表不进创世哈希——所以它照常入网。
awk '/^\[\[domains\]\]/{skip=3} skip>0{skip--; next} {print}' "$OUT/genesis.toml" > "$OUT/genesis-lagging.toml"
sed -i.bak "s|^genesis = .*|genesis = \"$OUT/genesis-lagging.toml\"|" "$OUT/n$LAG/node.toml"
lag_id="$("$BIN" genesis-hash --config "$OUT/n$LAG/node.toml")"
[ "$(echo "$lag_id" | head -1)" = "$BASE_HASH" ] \
  || { echo "没排期的节点创世哈希应当相同" >&2; exit 1; }
echo "  n$LAG:$(echo "$lag_id" | tail -1)(创世哈希相同)"

echo "== 启动 4 个验证人"
"$HERE/up.sh" >/dev/null
for id in 21 22 23 24; do wait_height "$id" 1 120; done

echo "== 1) H* 之前:领域集合里没有 $DOMAIN_ID,用它的单元被拒"
for id in 21 22 23 24; do
  echo "  n$id $(domains "$id")"
done
for id in 21 22 23 24; do
  case "$(domains "$id")" in
    *"\"$DOMAIN_ID\""*) echo "  n$id 的 next 里已经有 $DOMAIN_ID(还没到 H* 就不该有)" >&2; exit 1 ;;
  esac
done
CODE="$(create_in_domain 1 "$DOMAIN_ID")"
echo "  建单元的 HTTP 状态:$CODE"
[ "$CODE" = "422" ] || { echo "H* 之前用领域 $DOMAIN_ID 建单元应当被拒(422),实际 $CODE" >&2; exit 1; }
echo "  ✓ H* 之前:领域 $DOMAIN_ID 不可用"

echo "== 2) H* 之前:4 个节点同一条链"
for id in 21 22 23 24; do wait_height "$id" $((H - 1)) 120; done
same_block $((H - 1)) 21 22 23 24

echo "== 3) 越过 H*:3 个节点继续出块,n$LAG 停在 H-1"
for id in 21 22 23; do wait_height "$id" $((H + 8)) 120; done
same_block "$H" 21 22 23
same_block $((H + 8)) 21 22 23
sleep 5
lag_h="$(height "$LAG")"
echo "  n$LAG 高度 $lag_h(有排期的节点已到 $(height 21))"
[ "$lag_h" = $((H - 1)) ] || { echo "没排期的节点应停在 $((H - 1)),实际 $lag_h" >&2; exit 1; }
grep -qiE "StateRootMismatch|state.?root" "$OUT/n$LAG/node.log" \
  && echo "  n$LAG 日志:在 H* 报状态根不一致(停下,没有分叉)" \
  || echo "  (n$LAG 日志里没找到 StateRootMismatch 字样;高度已证明它停在 H-1)"

echo "== 4) H* 之后:领域 $DOMAIN_ID 可用"
for id in 21 22 23; do
  echo "  n$id $(domains "$id")"
done
case "$(domains 21)" in
  *"\"$DOMAIN_ID\""*) ;;
  *) echo "H* 之后 n21 的 next 里仍没有 $DOMAIN_ID" >&2; exit 1 ;;
esac
CODE="$(create_in_domain 1 "$DOMAIN_ID")"
echo "  建单元的 HTTP 状态:$CODE"
[ "$CODE" = "200" ] || { echo "H* 之后用领域 $DOMAIN_ID 建单元应当成功(200),实际 $CODE" >&2; exit 1; }
echo "  ✓ H* 之后:领域 $DOMAIN_ID 可用"

echo "== 5) 给 n$LAG 补上时刻表并重启:重放日志后追上"
kill -TERM "$(cat "$OUT/n$LAG/pid")"; sleep 3
cp "$OUT/genesis.toml" "$OUT/n$LAG/genesis.toml"
sed -i.bak "s|^genesis = .*|genesis = \"$OUT/n$LAG/genesis.toml\"|" "$OUT/n$LAG/node.toml"
"$HERE/up.sh" >/dev/null
target=$(( $(height 21) + 3 ))
wait_height "$LAG" "$target" 120
same_block "$H" 21 22 23 24
same_block "$target" 21 22 23 24

echo "== 6) 创世哈希不变(加领域不改链的身份)"
LAG_ID="$("$BIN" genesis-hash --config "$OUT/n$LAG/node.toml" | head -1)"
[ "$LAG_ID" = "$BASE_HASH" ] || { echo "创世哈希变了:$LAG_ID ≠ $BASE_HASH" >&2; exit 1; }
echo "  创世哈希仍是 $BASE_HASH"

echo "OK:加领域只是创世里的一行数据;创世哈希不变;H* 前领域不可用、H* 后可用;时刻表不一致的节点在 H* 停下而不分叉;补上时刻表后追上同一条链"
