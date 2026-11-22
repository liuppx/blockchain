#!/usr/bin/env bash
# 在新机(验证节点所在机器)上安装正文镜像:每小时把链上引用的全部正文复制到本机,并核对主正文仓库。
# 同时把带「正文镜像」检查的 monitor-testnet.sh 更新到两台机器。
#
#   bash deploy/deploy-body-mirror.sh
#
# 只装脚本和定时任务、跑一次并核对;不碰节点、链数据与密钥。
# 环境变量:HOST(默认 root@221.130.30.75,端口 10022)、OLD_HOST(默认 root@122.9.144.27)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOST="${HOST:-root@221.130.30.75}"
OLD_HOST="${OLD_HOST:-root@122.9.144.27}"
SSH=(ssh -p 10022 "$HOST")
DIR=/data/zhixing-testnet/bodies-mirror

echo "== 新机前置检查"
"${SSH[@]}" 'command -v python3 >/dev/null && python3 -c "import sys; assert sys.version_info >= (3, 8), sys.version" && curl -fsS --max-time 10 http://127.0.0.1:9221/height >/dev/null && echo "python3 $(python3 -V 2>&1 | cut -d" " -f2),本机节点 RPC 可答"'

echo "== 安装脚本"
"${SSH[@]}" "install -d -m 755 /opt/zhixing/tools /opt/zhixing/deploy && install -d -m 755 $DIR"
rsync -az -e "ssh -p 10022" "$ROOT/tools/body_mirror.py" "$HOST:/opt/zhixing/tools/body_mirror.py"
rsync -az -e "ssh -p 10022" "$ROOT/deploy/body-mirror.sh" "$ROOT/deploy/monitor-testnet.sh" "$HOST:/opt/zhixing/deploy/"
"${SSH[@]}" "chmod 755 /opt/zhixing/deploy/body-mirror.sh /opt/zhixing/deploy/monitor-testnet.sh"

echo "== 定时任务(每小时第 17 分)"
"${SSH[@]}" "cat > /etc/cron.d/zhixing-body-mirror <<'CRON'
# 知行图正文镜像:链上引用的全部正文与核验说明复制到 $DIR(仓库 deploy/body-mirror.sh)
17 * * * * root /opt/zhixing/deploy/body-mirror.sh
CRON
chmod 644 /etc/cron.d/zhixing-body-mirror"

echo "== 立即跑一次"
"${SSH[@]}" "/opt/zhixing/deploy/body-mirror.sh; rc=\$?; tail -6 /opt/logs/zhixing-body-mirror.log; ls $DIR | grep -cE '^[0-9a-f]{64}\$' | sed 's/^/副本文件数:/'; exit \$rc"

echo "== 新机监控(含正文检查)"
"${SSH[@]}" "/opt/zhixing/deploy/monitor-testnet.sh | grep -E 'bodies|ALERT' || true"

echo "== 旧机监控脚本同步(旧机没有副本目录,正文检查自动跳过)"
rsync -az "$ROOT/deploy/monitor-testnet.sh" "$OLD_HOST:/opt/zhixing/deploy/monitor-testnet.sh"
ssh "$OLD_HOST" "chmod 755 /opt/zhixing/deploy/monitor-testnet.sh && bash -n /opt/zhixing/deploy/monitor-testnet.sh && echo 旧机脚本已更新"
echo "== 完成"
