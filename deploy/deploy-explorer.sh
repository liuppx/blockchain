#!/usr/bin/env bash
# 更新旧机上的浏览器前端(知行图测试网)。
#
#   deploy/deploy-explorer.sh              # 同步 tools/explorer/ 并重启浏览器服务
#   deploy/deploy-explorer.sh --check      # 只看远端状态,不改动
#
# 旧机 122.9.144.27 只运行浏览器、正文仓库与 nginx;验证节点在 221.130.30.75
# (见 docs/design/spec-node-migration-v0.1.md)。本脚本**只碰浏览器**:
# 不碰节点、不碰链数据、不碰密钥。
#
# 环境变量:HOST(默认 root@122.9.144.27)、DEST(默认 /opt/zhixing/tools/explorer)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HOST="${HOST:-root@122.9.144.27}"
DEST="${DEST:-/opt/zhixing/tools/explorer}"
SERVICE=zhixing-explorer
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

SRC="$ROOT/tools/explorer"
[ -d "$SRC" ] || { echo "找不到 $SRC" >&2; exit 1; }

if [ "$CHECK_ONLY" = 1 ]; then
  echo "== 远端状态($HOST)"
  ssh "$HOST" "systemctl is-active $SERVICE; ls -1 $DEST/public/ | head -20"
  exit 0
fi

# 先看远端现在是什么样(便于事后核对)
echo "== 远端当前版本"
ssh "$HOST" "systemctl is-active $SERVICE; md5sum $DEST/public/member.js $DEST/public/util.js 2>/dev/null || true"

echo "== 同步 → $HOST:$DEST"
# --delete 让远端不留旧文件;排除本机依赖、测试与日志。
# reference/ 是 op 编码与签名的参考实现(只给测试用),不部署:服务器上不放任何私钥代码。
# scripts/ 是开发机上的构建脚本(如重新生成 public/vendor/three),也不部署。
# 注意:node_modules 不在远端安装(浏览器服务零依赖),排除它以免传垃圾。
rsync -az --delete \
  --exclude 'node_modules' \
  --exclude 'test' \
  --exclude 'reference' \
  --exclude 'scripts' \
  --exclude '*.log' \
  --exclude 'package-lock.json' \
  "$SRC/" "$HOST:$DEST/"

echo "== 重启并自检"
ssh "$HOST" "systemctl restart $SERVICE && sleep 2 && systemctl is-active $SERVICE"

# 自检:服务在跑,页面能返回,新代码确实到位
LOCAL_MEMBER="$(md5 -q "$SRC/public/member.js")"
REMOTE_MEMBER="$(ssh "$HOST" "md5sum $DEST/public/member.js | cut -d' ' -f1")"
if [ "$LOCAL_MEMBER" != "$REMOTE_MEMBER" ]; then
  echo "自检失败:member.js 与本地不一致(本地 $LOCAL_MEMBER / 远端 $REMOTE_MEMBER)" >&2
  exit 1
fi
HTTP="$(ssh "$HOST" "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/ || true")"
if [ "$HTTP" != "200" ]; then
  echo "自检失败:浏览器首页返回 $HTTP" >&2
  exit 1
fi
# 网页里不能有私钥代码:旧的签名器与词表必须已从服务器上消失
for gone in signer.js bip39-en.js; do
  CODE="$(ssh "$HOST" "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/$gone || true")"
  if [ "$CODE" != "404" ]; then
    echo "自检失败:/$gone 仍能下载(HTTP $CODE),网页里不应有私钥代码" >&2
    exit 1
  fi
done
echo "== 完成:浏览器已更新(member.js $LOCAL_MEMBER,首页 200,私钥代码已不在网页里)"
