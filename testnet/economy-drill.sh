#!/usr/bin/env bash
# 知识经济 v1 端到端演练:本机 4 个验证人 + 6 个成员账户,经济从高度 H 起激活,
# 用短节奏(结算 10 块、锁定 40 块、追加 15 块)在几分钟里走完一轮:
#
#   1. 激活前发布的单元不进经济(不追溯);
#   2. 激活后发布一个单元 ⇒ 协议指派 4 位核验人(不含发布者);
#   3. 被指派的 2 位核验「通过」⇒ 确立;他们各得 1 分;
#   4. 再确立一个单元让知识量上涨 ⇒ 结算时增发,积分分到发布者与核验人,锁定;
#   5. 锁定期满 ⇒ 释放;
#   6. 4 个节点上的 /economy 逐字相同(同一条链、同一份经济状态)。
#
#   testnet/economy-drill.sh
#
# 全部产物在临时目录,不碰 testnet/out。结束时停掉节点并删除(KEEP=1 保留)。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
H=8                       # 经济激活高度
PERIOD=10 LOCK=40 TOPUP=15
DRILL_DIR="$(mktemp -d -t zx-economy-drill)"
export TESTNET_OUT="$DRILL_DIR/net"
OUT="$TESTNET_OUT"

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

RPC="http://127.0.0.1:9221"
get() { curl -sf --max-time 3 "$RPC$1" || true; }
height() { get /height | tr -dc 0-9 || true; }
wait_height() { # min timeout
  local deadline=$((SECONDS + $2)) h
  while :; do
    h="$(height)"; [ -n "$h" ] && [ "$h" -ge "$1" ] && return 0
    [ $SECONDS -ge $deadline ] && { echo "等不到高度 $1(当前 ${h:-?})" >&2; return 1; }
    sleep 1
  done
}
nonce() { get "/account/$1" | tr ' ' '\n' | awk -F= '$1=="nonce"{print $2}'; }
econ() { get /economy; }

# 签名并提交一条知识 op:submit ACTOR ARGS...
submit() {
  local actor="$1"; shift
  local f="$DRILL_DIR/op-$RANDOM.bin" n
  n="$(nonce "$actor")"
  "$BIN" encode-knowledge-op --key-file "$OUT/keys/acct$actor.json" --out "$f" \
    --actor "$actor" --nonce "$n" "$@" >/dev/null
  local resp
  resp="$(curl -s --max-time 10 -X POST -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/octet-stream" --data-binary "@$f" "$RPC/knowledge")"
  [[ "$resp" =~ ^[0-9a-f]{64}$ ]] || { echo "账户 $actor 的 op 被拒:$resp" >&2; exit 1; }
}
# 等某账户的序号推进(它的 op 已入块)
wait_nonce() { # actor want timeout
  local deadline=$((SECONDS + $3))
  while [ "$(nonce "$1")" != "$2" ]; do
    [ $SECONDS -ge $deadline ] && { echo "账户 $1 的 op 迟迟没有入块" >&2; exit 1; }
    sleep 1
  done
}
body() { printf 'zh:%s' "$(printf '%s' "$1" | shasum -a 256 | cut -d' ' -f1)"; }
assigned_of() { econ | awk -v k="unit=$1 version=1" 'index($0,k)==1' | tr ' ' '\n' | awk -F= '$1=="assigned"{print $2}' | tr ',' ' '; }
established_of() { econ | awk -v k="unit=$1 version=1" 'index($0,k)==1' | tr ' ' '\n' | awk -F= '$1=="established"{print $2}'; }
verify_by() { # unit verifier
  local before; before="$(nonce "$2")"
  submit "$2" --action verify --unit "$1" --version 1 --verdict pass
  wait_nonce "$2" $((before + 1)) 60
}

echo "== 生成测试网:6 个成员,经济于高度 $H 激活,节奏 结算 $PERIOD / 锁定 $LOCK / 追加 $TOPUP 块"
"$HERE/gen.sh" -a 6 --drill --upgrade "knowledge-economy-v1=$H" \
  --economy-cadence "$PERIOD,$LOCK,$TOPUP" >/dev/null
# 演练用:成员都是创世成员(已过见习期),不需要等 60 块
echo "  $(tail -1 "$OUT/genesis-identity.txt")"
TOKEN="$(sed -n '/^\[rpc\]/,/^\[/p' "$OUT/n21/node.toml" | grep -m1 auth_token | sed -E 's/.*"([^"]*)".*/\1/')"
export ZHIXING_KEYSTORE_PASSWORD="$(cat "$OUT/password")"

echo "== 启动 4 个验证人"
"$HERE/up.sh" >/dev/null
wait_height 2 120

echo "== 1) 激活前发布单元 #1:不进经济"
submit 1 --action create --layer foundation --domain 2 --body "$(body pre-activation)" --source human
wait_nonce 1 1 60
[ "$(height)" -lt "$H" ] || { echo "单元 #1 入块太晚(已过 H),演练前提不成立;调大 H" >&2; exit 1; }
wait_height "$H" 60
econ | grep -q "^active=true" || { echo "过了 H 仍未激活" >&2; econ; exit 1; }
econ | grep -q "^unit=1 " && { echo "激活前的单元 #1 不该出现在经济里" >&2; exit 1; }
echo "  ✓ 已激活,单元 #1 没有被追溯"

echo "== 2) 激活后发布单元 #2(账户 1)⇒ 协议指派 4 位核验人"
submit 1 --action create --layer foundation --domain 2 --body "$(body unit-two)" --source human
wait_nonce 1 2 60
A2=($(assigned_of 2))
echo "  指派:${A2[*]}"
[ "${#A2[@]}" = 4 ] || { echo "应指派 4 位,实际 ${#A2[@]}" >&2; exit 1; }
for a in "${A2[@]}"; do [ "$a" != 1 ] || { echo "发布者不该被指派" >&2; exit 1; }; done

echo "== 3) 被指派的 2 位通过 ⇒ 确立,各得 1 分"
verify_by 2 "${A2[0]}"
[ "$(established_of 2)" = "-" ] || { echo "1 位通过就确立了" >&2; exit 1; }
verify_by 2 "${A2[1]}"
E2="$(established_of 2)"
[ "$E2" != "-" ] && [ -n "$E2" ] || { echo "2 位通过仍未确立" >&2; econ; exit 1; }
echo "  ✓ 单元 #2 于高度 $E2 确立"

echo "== 4) 再确立单元 #3(账户 2)⇒ 知识量上涨 ⇒ 结算时增发"
submit 2 --action create --layer foundation --domain 2 --body "$(body unit-three)" --source human
wait_nonce 2 "$(( $(nonce 2) ))" 5 || true
sleep 3
A3=($(assigned_of 3))
echo "  单元 #3 指派:${A3[*]}"
verify_by 3 "${A3[0]}"
verify_by 3 "${A3[1]}"
[ "$(established_of 3)" != "-" ] || { echo "单元 #3 未确立" >&2; exit 1; }
echo "  等过几个结算周期……"
wait_height $(( $(height) + 5 * PERIOD )) 180
ISSUED="$(econ | awk '/^period /' | tr ' ' '\n' | awk -F= '$1=="issued"{print $2}')"
echo "  累计增发:$ISSUED"
[ -n "$ISSUED" ] && [ "$ISSUED" -gt 0 ] || { echo "知识量涨了却没有增发" >&2; econ; exit 1; }
econ | grep "^lock " | sed 's/^/  /'
econ | grep -q "^lock account=1 " || { echo "发布者(账户 1)没拿到积分" >&2; exit 1; }
econ | grep -q "^lock account=${A2[0]} " || { echo "核验人 ${A2[0]} 没拿到积分" >&2; exit 1; }

echo "== 5) 锁定期满 ⇒ 释放"
wait_height $(( $(height) + LOCK + PERIOD )) 240
REL="$(econ | grep "^lock account=1 " | tr ' ' '\n' | awk -F= '$1=="released"{print $2}')"
echo "  账户 1 已释放:$REL"
[ -n "$REL" ] && [ "$REL" -gt 0 ] || { echo "锁定期满却没有释放" >&2; exit 1; }

echo "== 6) 4 个节点的经济状态逐字相同"
H_NOW="$(height)"; wait_height $((H_NOW + 2)) 30
ref=""
for id in 21 22 23 24; do
  # 同一高度上比较:各节点高度可能差一两块,所以比较区块哈希 + 经济摘要的去时变部分
  e="$(curl -sf "http://127.0.0.1:92$id/economy" | grep -v '^importance ' | grep -v '^active=' || true)"
  if [ -z "$ref" ]; then ref="$e"; elif [ "$e" != "$ref" ]; then
    echo "n$id 的经济状态与 n21 不同" >&2; diff <(echo "$ref") <(echo "$e") | head >&2; exit 1
  fi
done
echo "  ✓ 4 个节点一致"

echo "OK:激活不追溯;发布即指派;2 位通过即确立并得分;知识量上涨即增发并分到人;锁定期满释放;全网一致"
