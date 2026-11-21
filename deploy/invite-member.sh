#!/usr/bin/env bash
# 邀请成员加入知行图测试网:用一位已过见习期的成员的密钥签一条 invite,提交到链。
#
#   deploy/invite-member.sh --inviter <账户号> --key-file <seed 文件> --pubkey <新成员公钥>
#
# 规则(链上共识):只有已过见习期的成员能邀请;每位成员每 INVITE_COOLDOWN_BLOCKS 块最多邀请一人
# (测试网 86400 块 = 1 天)。邀请后新成员进入见习期(604800 块 = 7 天),见习期内核验照常记录、
# 但暂不计入可信度,也不能邀请他人。
#
# 令牌:从节点机器上现读(不落到本地);也可以用 RPC_TOKEN 环境变量直接给。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
NODE_BIN="${NODE_BIN:-$ROOT/node/target/release/node}"
RPC="${RPC:-https://blockchain.yeying.pub/rpc}"
SITE="${SITE:-https://blockchain.yeying.pub}"   # 浏览器服务:/lookup 在这里,不在节点 RPC 上
SSH_HOST="${SSH_HOST:-root@221.130.30.75}"
SSH=(ssh -p 10022 "$SSH_HOST")

INVITER=""
KEY_FILE=""
PUBKEY=""

usage() {
  cat >&2 <<'EOF'
Usage: deploy/invite-member.sh --inviter N --key-file FILE --pubkey HEX
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --inviter) INVITER="${2:?--inviter 需要一个值}"; shift 2 ;;
    --key-file) KEY_FILE="${2:?--key-file 需要一个值}"; shift 2 ;;
    --pubkey) PUBKEY="${2:?--pubkey 需要一个值}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done
[[ -n "$INVITER" && -n "$KEY_FILE" && -n "$PUBKEY" ]] || usage
[[ -f "$KEY_FILE" ]] || { echo "找不到密钥文件:$KEY_FILE" >&2; exit 1; }
PUBKEY="$(printf '%s' "$PUBKEY" | tr 'A-Z' 'a-z')"
[[ "$PUBKEY" =~ ^[0-9a-f]{64}$ ]] || { echo "公钥应为 64 位十六进制" >&2; exit 1; }

# 邀请人当前的序号与公钥
ACCT_JSON="$(curl -s --max-time 20 -H 'Accept: application/json' "$RPC/account/$INVITER")"
NONCE="$(printf '%s' "$ACCT_JSON" | sed -E 's/.*"nonce":"([0-9]+)".*/\1/')"
CUR_PK="$(printf '%s' "$ACCT_JSON" | sed -E 's/.*"pubkey":"([0-9a-f]+)".*/\1/')"
if [[ -z "$NONCE" || "$NONCE" == "$ACCT_JSON" ]]; then
  echo "读不到账户 #$INVITER 的序号" >&2; exit 1
fi
# 密钥文件里的公钥必须就是链上这个账户的公钥,否则签出来的东西链上不认
WORK="$(mktemp -d "${TMPDIR:-/tmp}/zhixing-invite.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
FILE_PK="$("$NODE_BIN" keygen --out "$WORK/check.seed" --seed "$(grep -v '^#' "$KEY_FILE" | tr -d '[:space:]')" 2>/dev/null | awk '$1=="pubkey"{print $2}')"
if [[ "$FILE_PK" != "$CUR_PK" ]]; then
  echo "密钥文件与链上账户 #$INVITER 对不上" >&2
  echo "  链上:$CUR_PK" >&2
  echo "  文件:$FILE_PK" >&2
  exit 1
fi
echo "邀请人:账户 #$INVITER(序号 $NONCE)"

# 签名(本地)
"$NODE_BIN" encode-knowledge-op --key-file "$KEY_FILE" --out "$WORK/invite.bin" \
  --actor "$INVITER" --nonce "$NONCE" --action invite --pubkey "$PUBKEY" >/dev/null

# 提交(令牌在节点机器上现读,本地不落盘)
if [[ -n "${RPC_TOKEN:-}" ]]; then
  TOKEN="$RPC_TOKEN"
else
  TOKEN="$("${SSH[@]}" "grep '^RPC_TOKEN=' /opt/deploy/zhixing-testnet/config/node21.toml | cut -d'\"' -f2")"
fi
RESP="$(curl -s --max-time 30 -X POST \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/octet-stream" \
  --data-binary "@$WORK/invite.bin" "$RPC/knowledge")"
echo "  提交结果:$RESP"

# 等入块:新成员的账户号
for _ in $(seq 1 60); do
  HIT="$(curl -s --max-time 20 "$SITE/lookup?pubkey=$PUBKEY")"
  case "$HIT" in
    *'"found":true'*)
      ACCT="$(printf '%s' "$HIT" | sed -E 's/.*"account":"?([0-9]+)"?.*/\1/')"
      echo "  ✓ 已入块,新成员是账户 #$ACCT"
      curl -s --max-time 20 "$RPC/members" | grep "^id=$ACCT " || true
      exit 0 ;;
  esac
  sleep 2
done
echo "超时:链上还没出现这个公钥" >&2
exit 1
