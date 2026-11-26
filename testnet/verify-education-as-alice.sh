#!/usr/bin/env bash
# 用 alice 的密钥在测试网上核验教育领域的 10 个单元。
#
# 说明(会随每条核验记录上链,原文存正文仓库):
#   这批单元由组织者代签,用于先验证「知识经济 v1」的指派、确立与结算流程能跑通。
#   alice 本人仍需按核验人指南逐条核对后重新给出结论。
#
# 口令从环境变量 ALICE_PW 取(在 ~/.zshrc 里);令牌从 ZHIXING_RPC_TOKEN 取。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KEY="${ALICE_KEY:-/Users/liuxin2/Downloads/zhixing-keystore-2026-11-26 (1).json}"
UNITS="${1:-17,18,19,20,21,22,23,24,25,26}"

export ZHIXING_KEYSTORE_PASSWORD="${ALICE_PW:?ALICE_PW 没设置}"
cd "$ROOT"
# --yes 是这个脚本显式的授权:跑这个脚本就等于同意按上面那份说明签发。
# 清单与说明仍会完整打印出来。
exec ./tools/zx --account 1 --key "$KEY" --yes verify-many \
  --unit "$UNITS" --note testnet/proxy-verification-note.txt
