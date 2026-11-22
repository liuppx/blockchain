#!/usr/bin/env bash
# 正文镜像的定时任务入口(新机,每小时):把链上引用的全部正文与核验说明复制到本机,并核对主仓库是否完整。
#
# 遵循社区运维规范:定时任务只调用脚本;脚本写日志、按大小截断;失败以非零码退出,调度器能看到。
# 是否完整由 monitor-testnet.sh 读 STATUS.json 判断并告警(本脚本不发通知,避免同一问题报两次)。
#
# 环境变量:
#   ZHIXING_RPC          读链上引用的 RPC(默认本机节点 http://127.0.0.1:9221,不经过旧机通道)
#   BODY_MIRROR_DIR      副本目录(默认 /data/zhixing-testnet/bodies-mirror)
#   BODY_SOURCE          主正文仓库(默认 https://blockchain.yeying.pub)
set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
TOOL="${BODY_MIRROR_TOOL:-$SCRIPT_DIR/../tools/body_mirror.py}"
LOGFILE="${LOGFILE:-/opt/logs/zhixing-body-mirror.log}"
export ZHIXING_RPC="${ZHIXING_RPC:-http://127.0.0.1:9221}"
DIR="${BODY_MIRROR_DIR:-/data/zhixing-testnet/bodies-mirror}"
SRC="${BODY_SOURCE:-https://blockchain.yeying.pub}"

mkdir -p "$(dirname "$LOGFILE")" "$DIR"
if [[ -f "$LOGFILE" ]] && [[ "$(stat -c %s "$LOGFILE" 2>/dev/null || echo 0)" -ge 1048576 ]]; then
  printf '[%s] truncated: log exceeded 1MB\n' "$(date)" > "$LOGFILE"
fi
{
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] 开始"
  python3 "$TOOL" --rpc "$ZHIXING_RPC" --source "$SRC" --dest "$DIR" --verify-source
  rc=$?
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] 结束 rc=$rc"
} >> "$LOGFILE" 2>&1
exit "${rc:-1}"
