#!/usr/bin/env bash
# 给测试网成员换密钥:把某个成员账户的公钥换成新生成的(标准派生)。
#
#   deploy/rotate-member-key.sh --account 1 --old-key /path/alice.seed --out /path/alice-v2.seed
#
# 做三件事:
#   1. 按**标准规则**(m/44'/1105'/0'/0'/0')生成新密钥,写到 --out;
#   2. 用**旧**密钥签一条换密钥 op(只有当前密钥有权换),提交到链;
#   3. 核对链上账户的公钥确实变成了新的。
#
# 换完之后:账户编号、成员身份、见习期与全部历史都不变;旧密钥立刻失效。
# 新密钥的助记词要交给成员本人(自己抄在纸上,或导入钱包)。
#
# 环境变量:
#   RPC         节点 RPC 地址(默认 https://blockchain.yeying.pub/rpc)
#   RPC_TOKEN   写入令牌(节点配了 auth_token 时必需)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NODE_BIN="${NODE_BIN:-$ROOT/node/target/release/node}"
RPC="${RPC:-https://blockchain.yeying.pub/rpc}"
RPC_TOKEN="${RPC_TOKEN:-}"

ACCOUNT=""
OLD_KEY=""
OUT=""
ACCOUNT_INDEX=0

usage() {
  cat >&2 <<'EOF'
Usage: deploy/rotate-member-key.sh --account N --old-key <seed 文件> --out <新 seed 文件>

  --account N      链上的账户编号
  --old-key FILE   当前(旧)密钥的 seed 文件,用来签名换密钥
  --out FILE       新密钥写到这里(带 0600 权限)
  --index I        新密钥用标准路径的第几个账户(默认 0)
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT="${2:?--account 需要一个值}"; shift 2 ;;
    --old-key) OLD_KEY="${2:?--old-key 需要一个值}"; shift 2 ;;
    --out) OUT="${2:?--out 需要一个值}"; shift 2 ;;
    --index) ACCOUNT_INDEX="${2:?--index 需要一个值}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done
[[ -n "$ACCOUNT" ]] || usage
[[ -n "$OLD_KEY" && -f "$OLD_KEY" ]] || { echo "找不到旧密钥文件:$OLD_KEY" >&2; exit 1; }
[[ -n "$OUT" ]] || usage
[[ -x "$NODE_BIN" ]] || { echo "找不到 node 二进制:$NODE_BIN" >&2; exit 1; }
[[ -n "$RPC_TOKEN" ]] || { echo "需要 RPC_TOKEN(节点的写入令牌)" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/zhixing-rotate.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

accounts_json() { curl -s --max-time 20 "$RPC/accounts"; }
field_of() { # field_of <账户号> <字段>
  accounts_json | awk -v a="$1" -v f="$2" '
    $1 == "id=" a { for (i = 1; i <= NF; i++) if ($i ~ "^" f "=") { split($i, kv, "="); print kv[2] } }'
}

CUR_PUBKEY="$(field_of "$ACCOUNT" pubkey)"
NONCE="$(field_of "$ACCOUNT" nonce)"
[[ -n "$CUR_PUBKEY" && -n "$NONCE" ]] || { echo "在链上找不到账户 #$ACCOUNT" >&2; exit 1; }

# 1. 新密钥(标准规则)
GEN_OUT="$("$NODE_BIN" keygen --out "$OUT" --new-mnemonic --account "$ACCOUNT_INDEX")"
NEW_PUBKEY="$(awk '$1=="pubkey"{print $2}' <<<"$GEN_OUT")"
chmod 600 "$OUT"
[[ -n "$NEW_PUBKEY" ]] || { echo "读不出新公钥" >&2; exit 1; }

echo "账户 #$ACCOUNT"
echo "  旧公钥 : $CUR_PUBKEY"
echo "  新公钥 : $NEW_PUBKEY"
echo "  当前序号: $NONCE"
if [[ "$CUR_PUBKEY" == "$NEW_PUBKEY" ]]; then
  echo "新公钥与链上的一样,不用换" >&2
  exit 1
fi

# 2. 用旧密钥签名并提交
"$NODE_BIN" encode-key-rotation --key-file "$OLD_KEY" --out "$WORK/rot.bin" \
  --account "$ACCOUNT" --nonce "$NONCE" --new-pubkey "$NEW_PUBKEY" >/dev/null

RESP="$(curl -s --max-time 30 -X POST \
  -H "Authorization: Bearer $RPC_TOKEN" -H "Content-Type: application/octet-stream" \
  --data-binary "@$WORK/rot.bin" "$RPC/key-rotation")"
echo "  提交结果: $RESP"
case "$RESP" in
  *"invalid signature"*|*"unauthorized"*|*"error"*) echo "提交被拒绝:$RESP" >&2; exit 1 ;;
esac

# 3. 核对链上
for _ in $(seq 1 30); do
  NOW="$(field_of "$ACCOUNT" pubkey)"
  if [[ "$NOW" == "$NEW_PUBKEY" ]]; then
    echo "  ✓ 链上公钥已更新"
    echo
    echo "新密钥: $OUT(600 权限)"
    echo "助记词:$(grep -m1 '^# mnemonic: ' "$OUT" | cut -d' ' -f3-)"
    echo
    echo "接下来:把新助记词交给成员本人,让他在钱包里「创建钱包 → 知行图钱包」导入(或用上面的 seed 文件),"
    echo "然后删掉旧的知行图账户。旧密钥从这一刻起失效。"
    exit 0
  fi
  sleep 2
done

echo "超时:链上公钥仍是 $NOW(期望 $NEW_PUBKEY)" >&2
exit 1
