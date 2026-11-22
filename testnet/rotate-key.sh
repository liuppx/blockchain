#!/usr/bin/env bash
# 换密钥:把一个知行图账户的公钥换成新的(账户编号、成员身份、见习期与全部历史不变)。
#
#   testnet/rotate-key.sh --account 1 --old-key <seed 文件> --new-mnemonic "词 词 …"
#
# 做三件事:
#   1. 按**标准规则**(m/44'/1105'/0'/0'/0')从助记词派生新密钥;
#   2. 用**旧**密钥签一条换密钥 op(旧密钥才有权限换);
#   3. 提交到节点,并核对链上账户的公钥确实变成了新的。
#
# 换完之后:旧密钥立刻失效;新密钥要自己放进钱包(「创建钱包 → 知行图钱包」或导入)。
#
# 环境变量:
#   NODE_BIN     node 二进制(默认 node/target/release/node)
#   RPC          节点 RPC 地址(默认 http://127.0.0.1:9221)
#   RPC_TOKEN    写入令牌(节点配了 auth_token 时必需)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NODE_BIN="${NODE_BIN:-$ROOT/node/target/release/node}"
RPC="${RPC:-http://127.0.0.1:9221}"
RPC_TOKEN="${RPC_TOKEN:-}"

ACCOUNT=""
OLD_KEY=""
NEW_MNEMONIC=""
ACCOUNT_INDEX=0

usage() {
  cat >&2 <<'EOF'
Usage: testnet/rotate-key.sh --account N --old-key <seed 文件> --new-mnemonic "24 个词"

  --account N         链上的账户编号(alice 是 1,bob 是 2)
  --old-key FILE      当前(旧)密钥的 seed 文件,用来签名换密钥
  --new-mnemonic "…"  新助记词(24 个词)。留空则新生成一组
  --index I           新密钥用标准路径的第几个账户(默认 0)
EOF
  exit 2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT="${2:?--account 需要一个值}"; shift 2 ;;
    --old-key) OLD_KEY="${2:?--old-key 需要一个值}"; shift 2 ;;
    --new-mnemonic) NEW_MNEMONIC="${2:?--new-mnemonic 需要一个值}"; shift 2 ;;
    --index) ACCOUNT_INDEX="${2:?--index 需要一个值}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done
[[ -n "$ACCOUNT" ]] || usage
[[ -n "$OLD_KEY" && -f "$OLD_KEY" ]] || { echo "找不到旧密钥文件:$OLD_KEY" >&2; exit 1; }
[[ -x "$NODE_BIN" ]] || { echo "找不到 node 二进制:$NODE_BIN(先 cargo build --release)" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/zhixing-rotate.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

# 1. 新密钥
NEW_SEED="$WORK/new.seed"
if [[ -n "$NEW_MNEMONIC" ]]; then
  GEN_OUT="$("$NODE_BIN" keygen --out "$NEW_SEED" --mnemonic "$NEW_MNEMONIC" --account "$ACCOUNT_INDEX")"
else
  GEN_OUT="$("$NODE_BIN" keygen --out "$NEW_SEED" --new-mnemonic --account "$ACCOUNT_INDEX")"
fi
NEW_PUBKEY="$(awk '$1=="pubkey"{print $2}' <<<"$GEN_OUT")"
[[ -n "$NEW_PUBKEY" ]] || { echo "读不出新公钥" >&2; exit 1; }

# 2. 当前序号
NONCE="$(curl -s --max-time 10 "$RPC/accounts" | awk -v a="$ACCOUNT" '
  $1 == "id=" a { for (i = 1; i <= NF; i++) if ($i ~ /^nonce=/) { split($i, kv, "="); print kv[2] } }')"
[[ -n "$NONCE" ]] || { echo "在链上找不到账户 #$ACCOUNT" >&2; exit 1; }

CUR_PUBKEY="$(curl -s --max-time 10 "$RPC/accounts" | awk -v a="$ACCOUNT" '
  $1 == "id=" a { for (i = 1; i <= NF; i++) if ($i ~ /^pubkey=/) { split($i, kv, "="); print kv[2] } }')"

echo "账户 #$ACCOUNT"
echo "  旧公钥 : $CUR_PUBKEY"
echo "  新公钥 : $NEW_PUBKEY"
echo "  当前序号: $NONCE"
if [[ "$CUR_PUBKEY" == "$NEW_PUBKEY" ]]; then
  echo "新公钥与链上的一样,不用换" >&2
  exit 1
fi

# 3. 签名并提交
"$NODE_BIN" encode-key-rotation --key-file "$OLD_KEY" --out "$WORK/rot.bin" \
  --account "$ACCOUNT" --nonce "$NONCE" --new-pubkey "$NEW_PUBKEY" >/dev/null

HEADERS=(-H "Content-Type: application/octet-stream")
[[ -n "$RPC_TOKEN" ]] && HEADERS+=(-H "Authorization: Bearer $RPC_TOKEN")
RESP="$(curl -s --max-time 20 -X POST "${HEADERS[@]}" --data-binary "@$WORK/rot.bin" "$RPC/key-rotation")"
echo "  提交结果: $RESP"
case "$RESP" in
  *"invalid signature"*|*"error"*|*"not found"*) echo "提交被拒绝:$RESP" >&2; exit 1 ;;
esac

# 4. 核对链上确实换了
for _ in $(seq 1 30); do
  NOW="$(curl -s --max-time 10 "$RPC/accounts" | awk -v a="$ACCOUNT" '
    $1 == "id=" a { for (i = 1; i <= NF; i++) if ($i ~ /^pubkey=/) { split($i, kv, "="); print kv[2] } }')"
  if [[ "$NOW" == "$NEW_PUBKEY" ]]; then
    echo "  ✓ 链上公钥已更新"
    echo
    echo "接下来:在钱包里导入新密钥(「创建钱包 → 知行图钱包」或「导入钱包 → 知行图」),"
    echo "并删掉旧的知行图账户。新助记词:$(grep -m1 '^# mnemonic: ' "$NEW_SEED" | cut -d' ' -f3-)"
    exit 0
  fi
  sleep 2
done

echo "超时:链上公钥仍是 $NOW(期望 $NEW_PUBKEY)" >&2
exit 1
