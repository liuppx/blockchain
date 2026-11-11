#!/usr/bin/env bash
# 测试网磁盘看守(systemd 定时器每 10 分钟运行一次)。
#
# 1. 估算剩余天数:剩余空间 ÷ 链数据的增长速度(两次运行之间 4 个节点 data 目录的增量),
#    写入 /opt/zhixing/status/disk.json,浏览器据此在页面顶部显示提醒;
# 2. 剩余空间低于 STOP_FREE_MB 时,优雅停止 4 个验证节点(SIGTERM)。
#    这台服务器还运行着其他项目(MySQL 等),磁盘写满会连带它们出错;
#    停掉测试网只让链暂停,数据完整,腾出空间后 systemctl start 即可恢复。
set -euo pipefail

DATA=${DATA:-/opt/zhixing/testnet/out}
STATUS_DIR=${STATUS_DIR:-/opt/zhixing/status}
STATE=${STATE:-/var/lib/zhixing-diskwatch.state}
DRY_RUN=${DRY_RUN:-0}   # 1 = 只记录"将要停止",不真的停(用于演练)
WARN_DAYS=${WARN_DAYS:-14}
STOP_FREE_MB=${STOP_FREE_MB:-1500}
UNITS="zhixing-node@21 zhixing-node@22 zhixing-node@23 zhixing-node@24"

now=$(date +%s)
free_mb=$(df -Pm / | awk 'NR==2 {print $4}')
used_mb=$(du -sm "$DATA" | awk '{print $1}')

rate=0   # MB/天
if [ -f "$STATE" ]; then
  read -r t0 u0 r0 < "$STATE" || true
  dt=$(( now - ${t0:-now} ))
  if [ "$dt" -ge 300 ]; then
    rate=$(awk -v du="$(( used_mb - u0 ))" -v dt="$dt" 'BEGIN { r = du * 86400 / dt; print (r > 0 ? r : 0) }')
    # 平滑:与上次的估计取加权平均,避免单次波动
    if [ -n "${r0:-}" ] && [ "${r0}" != "0" ]; then
      rate=$(awk -v a="$rate" -v b="$r0" 'BEGIN { print 0.3 * a + 0.7 * b }')
    fi
  else
    rate=${r0:-0}
  fi
fi
echo "$now $used_mb $rate" > "$STATE"

days=$(awk -v f="$free_mb" -v s="$STOP_FREE_MB" -v r="$rate" 'BEGIN { if (r <= 0) print -1; else printf "%.1f", (f - s) / r }')
level=ok
if awk -v d="$days" -v w="$WARN_DAYS" 'BEGIN { exit !(d >= 0 && d < w) }'; then level=warn; fi

stopped=false
if [ "$free_mb" -lt "$STOP_FREE_MB" ]; then
  level=stopped
  if systemctl is-active --quiet zhixing-node@21; then
    logger -p user.crit -t zhixing-diskwatch "free ${free_mb}MB < ${STOP_FREE_MB}MB: stopping testnet validators to protect the host"
    if [ "$DRY_RUN" = 1 ]; then echo "DRY_RUN: would run: systemctl stop $UNITS"; else systemctl stop $UNITS; fi
  fi
  stopped=true
elif [ "$level" = warn ]; then
  logger -p user.warning -t zhixing-diskwatch "about ${days} days of disk left (free ${free_mb}MB, chain grows ${rate}MB/day)"
fi

install -d -m 755 "$STATUS_DIR"
tmp=$(mktemp "$STATUS_DIR/.disk.XXXXXX")
printf '{"checked_at":%s,"free_mb":%s,"chain_mb":%s,"growth_mb_per_day":%s,"days_left":%s,"stop_free_mb":%s,"level":"%s","validators_stopped":%s}\n' \
  "$now" "$free_mb" "$used_mb" "$rate" "$days" "$STOP_FREE_MB" "$level" "$stopped" > "$tmp"
chmod 644 "$tmp"
mv "$tmp" "$STATUS_DIR/disk.json"
