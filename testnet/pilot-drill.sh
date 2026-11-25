#!/usr/bin/env bash
# 试运行全流程演练:在本机 4 节点 + 6 个成员账户上,把试运行要走的一整遍走通。
#
#   testnet/pilot-drill.sh
#
# 走通的环节:
#   1. 公布埋错清单的 SHA-256(先承诺);
#   2. 组织者把埋错发布为新版本(发布工具 pilot_plant.py);
#   3. 协议指派核验人;被指派的成员核验(有的发现错误、有的没发现);
#   4. 核对:核验记录、确立事件、得分、结算;
#   5. 指标工具(pilot_metrics.py)读出发现率、误报率,并用答案文件核对承诺;
#   6. 组织者发布更正版本;核对更正后的可信度变化。
#
# 这是"演练",不是试运行:用的账户、单元、错误都是假的,为的是验证**流程**本身没断。
# 全部产物在临时目录,不碰 testnet/out。结束时停掉节点并删除(KEEP=1 保留)。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BIN="$ROOT/node/target/release/node"
DRILL_DIR="$(mktemp -d -t zx-pilot-drill)"
export TESTNET_OUT="$DRILL_DIR/net"
OUT="$TESTNET_OUT"
H=6                       # 经济激活高度
PERIOD=6 LOCK=30 TOPUP=9
RPC="http://127.0.0.1:9221"
SITE="http://127.0.0.1:18089"

cleanup() {
  # 停浏览器服务并等它退出(wait 吞掉 bash 的 "Terminated" 提示)
  if [ -f "$DRILL_DIR/explorer.pid" ]; then
    kill "$(cat "$DRILL_DIR/explorer.pid")" 2>/dev/null || true
    wait "$(cat "$DRILL_DIR/explorer.pid")" 2>/dev/null || true
  fi
  "$HERE/down.sh" >/dev/null 2>&1 || true
  if [ "${KEEP:-0}" = 1 ]; then echo "(保留演练目录 $DRILL_DIR)"; else rm -rf "$DRILL_DIR"; fi
}
trap cleanup EXIT

[ -x "$BIN" ] || { echo "先构建:cargo build --release --manifest-path node/Cargo.toml" >&2; exit 1; }
for id in 21 22 23 24; do
  if command -v lsof >/dev/null && lsof -nP -iTCP:"90$id" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "端口 90$id 被占用:本机已有测试网在跑?先 testnet/down.sh" >&2; exit 1
  fi
done
if command -v lsof >/dev/null && lsof -nP -iTCP:18089 -sTCP:LISTEN >/dev/null 2>&1; then
  echo "端口 18089 被占用(遗留的浏览器服务?):$(lsof -nP -iTCP:18089 -sTCP:LISTEN | tail -1)" >&2; exit 1
fi

get() { curl -sf --max-time 3 "$RPC$1" || true; }
height() { get /height | tr -dc 0-9 || true; }
nonce() { get "/account/$1" | tr ' ' '\n' | awk -F= '$1=="nonce"{print $2}'; }
wait_height() { local d=$((SECONDS + $2)); while :; do h=$(height); [ -n "$h" ] && [ "$h" -ge "$1" ] && return 0; [ $SECONDS -ge $d ] && { echo "等不到高度 $1" >&2; return 1; }; sleep 1; done; }
wait_nonce() { local d=$((SECONDS + $3)); while [ "$(nonce "$1")" != "$2" ]; do [ $SECONDS -ge $d ] && { echo "账户 $1 的 op 没入块" >&2; return 1; }; sleep 1; done; }
# 提交一条知识 op:submit ACTOR ARGS...
submit() {
  local actor="$1"; shift
  local f="$DRILL_DIR/op-$RANDOM.bin" n resp a prev=""
  # --body 的值来自 $(need_body …):上传失败时是空串。在这里检查,因为子 shell 里 exit 不了主流程。
  for a in "$@"; do
    if [ "$prev" = "--body" ] && ! [[ "$a" =~ ^zh:[0-9a-f]{64}$ ]]; then
      echo "账户 $actor:正文没上传成功(--body 是 \"$a\"),演练中止" >&2; exit 1
    fi
    prev="$a"
  done
  n="$(nonce "$actor")"
  "$BIN" encode-knowledge-op --key-file "$OUT/keys/acct$actor.json" --out "$f" \
    --actor "$actor" --nonce "$n" "$@" >/dev/null
  resp="$(curl -s --max-time 10 -X POST -H "Authorization: Bearer $TOKEN" \
    -H "Content-Type: application/octet-stream" --data-binary "@$f" "$RPC/knowledge")"
  [[ "$resp" =~ ^[0-9a-f]{64}$ ]] || { echo "账户 $actor 的 op 被拒:$resp" >&2; exit 1; }
  wait_nonce "$actor" $((n + 1)) 60
}
# 正文:先上传到正文仓库(按哈希寻址),再把哈希上链——与线上一致
# 注意 body 在 $(...) 里调用,里面的 exit 只结束子 shell——所以失败时什么都不回显,
# 由 create/publish 的调用方检查"拿到的是不是 zh:<哈希>"。
body() {
  local text="$1" h
  h="$(printf '%s' "$text" | shasum -a 256 | cut -d' ' -f1)"
  printf '%s' "$text" | curl -sf -X PUT -H "Authorization: Bearer $BODY_TOKEN" \
    -H "Content-Type: text/markdown; charset=utf-8" --data-binary @- "$SITE/bodies/$h" >/dev/null \
    || { echo "正文上传失败(哈希 ${h:0:12})" >&2; return 1; }
  printf 'zh:%s' "$h"
}
need_body() { body "$1"; }
assigned_of() { curl -sf "$SITE/assign?unit=$1&version=1" | python3 -c "import json,sys;print(' '.join(json.load(sys.stdin).get('assigned',[])))" 2>/dev/null || true; }
established_of() { curl -sf "$SITE/assign?unit=$1&version=1" | python3 -c "import json,sys;d=json.load(sys.stdin);print(d.get('established_height') or '-')" 2>/dev/null || echo "-"; }

echo "== 生成测试网(6 个成员,经济于高度 $H 激活,节奏 $PERIOD/$LOCK/$TOPUP)"
"$HERE/gen.sh" -a 6 --drill --upgrade "knowledge-economy-v1=$H" \
  --economy-cadence "$PERIOD,$LOCK,$TOPUP" >/dev/null
TOKEN="$(sed -n '/^\[rpc\]/,/^\[/p' "$OUT/n21/node.toml" | grep -m1 auth_token | sed -E 's/.*"([^"]*)".*/\1/')"
export ZHIXING_KEYSTORE_PASSWORD="$(cat "$OUT/password")"

echo "== 起浏览器服务(带正文仓库:埋错工具要从这里取正文;指标工具要读它的 /assign)"
mkdir -p "$DRILL_DIR/bodies"
# 与线上一致:正文仓库的上传令牌就是 RPC 写令牌(deploy/systemd/zhixing-explorer.service)
BODY_TOKEN="$TOKEN"
export ZHIXING_RPC_TOKEN="$TOKEN"   # pilot_plant.py 发布时用(写入令牌)
( cd "$ROOT/tools/explorer" && exec env PORT=18089 NODE_RPC="$RPC" BODY_DIR="$DRILL_DIR/bodies" BODY_TOKEN="$BODY_TOKEN" node server.js ) >"$DRILL_DIR/explorer.log" 2>&1 &
EXP_PID=$!; echo "$EXP_PID" > "$DRILL_DIR/explorer.pid"
for _ in $(seq 1 20); do curl -sf "$SITE/" >/dev/null && break; sleep 0.5; done

echo "== 起 4 个验证人"
"$HERE/up.sh" >/dev/null
wait_height 2 120

# ---------------------------------------------------------------- 1. 先承诺
echo
echo "== 1. 公布埋错清单的 SHA-256(先承诺,后公开)"
PLANTS="$DRILL_DIR/plants.json"
cat > "$PLANTS" <<'JSON'
{
  "plants": [
    {"id": "F1", "unit": 2, "old": "组间休息 60–90 秒。", "new": "组间休息 10–15 秒。", "note": "A 明显"},
    {"id": "F2", "unit": 3, "old": "休息间隔 48 小时。", "new": "休息间隔 12 小时。", "note": "A 明显"}
  ],
  "red_team": []
}
JSON
COMMIT="$(shasum -a 256 "$PLANTS" | cut -d' ' -f1)"
echo "  承诺的 SHA-256:$COMMIT"

# ---------------------------------------------------------------- 2. 正常内容 + 埋错
echo
echo "== 2. 等经济激活,再发布单元(激活前发布的单元不进经济,不会有链上指派)"
wait_height "$H" 60
curl -sf -H 'Accept: application/json' "$RPC/economy" | grep -q '"active":true' \
  || { echo "过了激活高度经济仍未激活" >&2; exit 1; }
submit 1 --action create --layer foundation --domain 2 --body "$(need_body '每周三次,每次 30 分钟。')" --source human
submit 2 --action create --layer foundation --domain 2 --body "$(need_body '每组之间:组间休息 60–90 秒。')" --source human
submit 3 --action create --layer foundation --domain 2 --body "$(need_body '同一肌群:休息间隔 48 小时。')" --source human
echo "  单元 #1(干净)、#2(有 F1)、#3(有 F2)已在激活后发布"

# 组织者(账户 3)把埋错发布为新版本:先检查(只读),再发布
python3 "$ROOT/tools/pilot_plant.py" --plants "$PLANTS" --rpc "$RPC" --site "$SITE" \
  --out "$DRILL_DIR/key.json" 2>&1 | sed 's/^/  /'
echo "  (上面是检查:埋错能不能干净地应用)"
# 再真的发布:组织者用账户 3,把 F1 发布为单元 #2 的新版本
echo "  组织者(账户 3)把 F1 发布为单元 #2 的新版本:"
python3 "$ROOT/tools/pilot_plant.py" --plants "$PLANTS" --rpc "$RPC" --site "$SITE" \
  --publish --key "3=$OUT/keys/acct3.json" --out "$DRILL_DIR/key.json" 2>&1 | sed 's/^/  /'
[ -f "$DRILL_DIR/key.json" ] || { echo "答案文件没写出来" >&2; exit 1; }

# ---------------------------------------------------------------- 3. 核验
echo
echo "== 3. 协议指派核验人,被指派的成员核验"
for u in 1 2 3; do
  A=($(assigned_of $u))
  SRC="$(curl -sf "$SITE/assign?unit=$u&version=1" | python3 -c "import json,sys;print(json.load(sys.stdin).get('source'))")"
  echo "  单元 #$u 指派给:${A[*]:-无}(来源:$SRC)"
  [ "$SRC" = "chain" ] || { echo "单元 #$u 的指派应当来自链上(它在激活后发布)" >&2; exit 1; }
done
# 单元 #2 有埋错 F1:被指派的核验人给出「需修改」(发现了)
A2=($(assigned_of 2))
if [ "${#A2[@]}" -gt 0 ]; then
  submit "${A2[0]}" --action verify --unit 2 --version 2 --verdict needs-revision
  echo "  ${A2[0]} 核验单元 #2 的 v2:需修改(发现了 F1)"
fi
# 单元 #3 有埋错 F2,但核验人没发现 ⇒ 误报/漏报由指标工具算
A3=($(assigned_of 3))
if [ "${#A3[@]}" -gt 0 ]; then
  submit "${A3[0]}" --action verify --unit 3 --version 2 --verdict pass
  echo "  ${A3[0]} 核验单元 #3 的 v2:通过(没发现 F2)"
fi
# 单元 #1 干净:两位通过 ⇒ 确立
A1=($(assigned_of 1))
if [ "${#A1[@]}" -gt 0 ]; then
  submit "${A1[0]}" --action verify --unit 1 --version 1 --verdict pass
  echo "  ${A1[0]} 核验单元 #1:通过"
fi
if [ "${#A1[@]}" -gt 1 ]; then
  submit "${A1[1]}" --action verify --unit 1 --version 1 --verdict pass
fi

# ---------------------------------------------------------------- 4. 核对
echo
echo "== 4. 核对:确立、得分、结算"
E1="$(established_of 1)"; E2="$(established_of 2)"
echo "  单元 #1 确立高度:$E1(应当有值:2 位被指派的核验人通过)"
echo "  单元 #2 确立高度:$E2(应当是 -:只有 1 位需修改)"
[ "$E1" != "-" ] || { echo "单元 #1 两位被指派的核验人都通过了,却没有确立" >&2; exit 1; }
[ "$E2" = "-" ] || { echo "单元 #2 只有 1 位需修改,不该确立" >&2; exit 1; }
wait_height $(( $(height) + 2 * PERIOD )) 180
SETTLED="$(curl -sf -H 'Accept: application/json' "$RPC/economy" | python3 -c "import json,sys;print(json.load(sys.stdin)['period']['settled'])")"
ISSUED="$(curl -sf -H 'Accept: application/json' "$RPC/economy" | python3 -c "import json,sys;print(json.load(sys.stdin)['period']['issued'])")"
echo "  已结算 $SETTLED 期,累计增发 $ISSUED"
[ "$SETTLED" -gt 0 ] || { echo "过了几个周期却没有结算" >&2; exit 1; }
[ "$ISSUED" -gt 0 ] || { echo "单元 #1 确立后知识量上涨,应当有增发" >&2; exit 1; }

# ---------------------------------------------------------------- 5. 指标
echo
echo "== 5. 指标工具读出发现率与误报率"
python3 "$ROOT/tools/pilot_metrics.py" --rpc "$RPC" --site "$SITE" \
  --since-height "$H" --seconds-per-block 1.19 \
  --plants "$PLANTS" --commitment "$COMMIT" --answer-key "$DRILL_DIR/key.json" 2>&1 | sed 's/^/  /'

echo "  (上面:先核对清单与承诺一致、答案文件按清单发布,再算发现率与误报率)"
M="$(python3 "$ROOT/tools/pilot_metrics.py" --rpc "$RPC" --site "$SITE" --since-height "$H" \
  --plants "$PLANTS" --commitment "$COMMIT" --answer-key "$DRILL_DIR/key.json" 2>&1)"
echo "$M" | grep -q "埋错版本被发现" || { echo "指标里没有发现率" >&2; exit 1; }
echo "$M" | grep -q "| 运动健身 | 1/2(50%) | 1/2(50%) |" \
  || { echo "发现率不对:应为 2 个埋错版本抓到 1 个、埋错版本上 2 次核验 1 次需修改" >&2; echo "$M" | grep "运动健身" >&2; exit 1; }
echo "  ✓ 发现率 1/2、埋错版本上的需修改 1/2:与演练里安排的一致"
# 篡改答案文件:把埋错版本指向一个不是按清单发布的版本 ⇒ 必须被拒
python3 - "$DRILL_DIR/key.json" "$DRILL_DIR/key-forged.json" <<'PYEOF'
import json, sys
k = json.load(open(sys.argv[1]))
k["planted"][0]["version"] = 1          # 指向原始的干净版本
json.dump(k, open(sys.argv[2], "w"))
PYEOF
if python3 "$ROOT/tools/pilot_metrics.py" --rpc "$RPC" --site "$SITE" --since-height "$H" \
     --plants "$PLANTS" --commitment "$COMMIT" --answer-key "$DRILL_DIR/key-forged.json" >/dev/null 2>&1; then
  echo "篡改过的答案文件居然被接受了" >&2; exit 1
fi
echo "  ✓ 篡改过的答案文件(把埋错指向别的版本)被拒绝"
# ---------------------------------------------------------------- 6. 更正
echo
echo "== 6. 组织者发布更正版本"
# 更正:往原单元发新版本(账户 1 是单元 #2 的作者)
submit 1 --action publish --unit 2 --body "$(need_body '组间休息 60–90 秒(更正)。休息间隔 48 小时(更正)。')" --source human
echo "  已发布更正版本;单元 #2 现在有 2 个版本"

echo
echo "OK:承诺 → 发布埋错 → 协议指派 → 核验 → 确立与得分 → 结算 → 指标读出 → 更正,整条链走通"
