#!/usr/bin/env bash
# 更新旧机上的浏览器前端(知行图测试网)。
#
#   deploy/deploy-explorer.sh              # 拉社区 main → 部署 → 重启 → 自检
#   deploy/deploy-explorer.sh --check      # 只看远端状态,不改动
#
# 旧机 122.9.144.27 只运行浏览器、正文仓库与 nginx;验证节点在 221.130.30.75
# (见 docs/design/spec-node-migration-v0.1.md)。本脚本**只碰浏览器**:
# 不碰节点、不碰链数据、不碰密钥。
#
# 发布约定:代码先合并到社区 main,服务器上拉社区 main 的源码部署——
# 不用开发机 rsync 过去的文件。浏览器服务是纯 Node(零依赖),部署只是换一份源码。
#
# 环境变量:
#   HOST       旧机(默认 root@122.9.144.27)
#   REPO       社区仓库(默认 https://github.com/yeying-community/blockchain.git)
#   CODE_DIR   旧机上的源码目录(默认 /root/code/blockchain)
#   DEST       浏览器服务目录(默认 /opt/zhixing/tools/explorer)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HOST="${HOST:-root@122.9.144.27}"
REPO="${REPO:-https://github.com/yeying-community/blockchain.git}"
CODE_DIR="${CODE_DIR:-/root/code/blockchain}"
DEST="${DEST:-/opt/zhixing/tools/explorer}"
SERVICE=zhixing-explorer
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

info() { printf '\n== %s\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

if [ "$CHECK_ONLY" = 1 ]; then
  info "远端状态($HOST)"
  ssh "$HOST" "systemctl is-active $SERVICE; ls -1 $DEST/public/ | head -20; readlink -f $CODE_DIR 2>/dev/null || true"
  exit 0
fi

info "旧机当前版本"
ssh "$HOST" "systemctl is-active $SERVICE; md5sum $DEST/public/member.js $DEST/public/util.js 2>/dev/null || true"

info "旧机拉取社区 main($REPO)"
# 源码放在旧机上,部署时直接从社区 main 取——与节点机同一套办法。
ssh "$HOST" "set -e
  if [ ! -d '$CODE_DIR/.git' ]; then
    mkdir -p \$(dirname '$CODE_DIR')
    git clone '$REPO' '$CODE_DIR'
  fi
  cd '$CODE_DIR'
  git remote set-url origin '$REPO'
  git fetch --prune origin main
  if [ -n \"\$(git status --porcelain)\" ]; then
    echo '旧机源码工作区有未提交改动,拒绝拉取(先在旧机上处理):' >&2
    git status --short >&2
    exit 1
  fi
  git checkout -q main 2>/dev/null || git checkout -q -B main origin/main
  git reset --hard origin/main
  git clean -fdq
  echo \"  HEAD = \$(git rev-parse --short HEAD)\"
  echo \"  提交标题:\$(git log -1 --pretty=%s)\""

COMMIT="$(ssh "$HOST" "cd '$CODE_DIR' && git rev-parse --short HEAD")"
info "部署 $COMMIT → $DEST"
ssh "$HOST" "set -e
  rm -rf '$DEST.new'
  mkdir -p '$DEST.new'
  cp -R '$CODE_DIR/tools/explorer/.' '$DEST.new/'
  # 浏览器服务零依赖:不装 node_modules;测试与参考签名器(带私钥代码)不上服务器
  rm -rf '$DEST.new/node_modules' '$DEST.new/test' '$DEST.new/reference' '$DEST.new/scripts'
  rm -f '$DEST.new/package-lock.json'
  # 目录换掉:服务重启后指向新版本,旧的留在旁边便于回退
  rm -rf '$DEST.old'
  [ -d '$DEST' ] && mv '$DEST' '$DEST.old'
  mv '$DEST.new' '$DEST'
  chown -R zhixing:zhixing '$DEST' 2>/dev/null || true
  rm -rf '$DEST.old'
  echo '  装好了'"

info "重启并自检"
ssh "$HOST" "systemctl restart $SERVICE && sleep 2 && systemctl is-active $SERVICE"

REMOTE_MEMBER="$(ssh "$HOST" "md5sum $DEST/public/member.js | cut -d' ' -f1")"
SOURCE_MEMBER="$(ssh "$HOST" "md5sum '$CODE_DIR/tools/explorer/public/member.js' | cut -d' ' -f1")"
if [ "$REMOTE_MEMBER" != "$SOURCE_MEMBER" ]; then
  fail "member.js 与社区 main 的源码不一致(部署 $REMOTE_MEMBER / 源码 $SOURCE_MEMBER)"
fi
HTTP="$(ssh "$HOST" "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/ || true")"
if [ "$HTTP" != "200" ]; then
  fail "浏览器首页返回 $HTTP"
fi
# 网页里不能有私钥代码:旧的签名器与词表必须已从服务器上消失
for gone in signer.js bip39-en.js; do
  CODE="$(ssh "$HOST" "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/$gone || true")"
  if [ "$CODE" != "404" ]; then
    fail "/$gone 仍能下载(HTTP $CODE),网页里不应有私钥代码"
  fi
done
# 新加的页面(立体地图、经济面板)也要在
for page in map3d.js terrain.js; do
  CODE="$(ssh "$HOST" "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080/$page || true")"
  if [ "$CODE" != "200" ]; then
    fail "/$page 返回 $CODE(部署不完整)"
  fi
done
echo "== 完成:浏览器已更新到社区 main $COMMIT(member.js $REMOTE_MEMBER,首页 200,私钥代码不在网页里)"
