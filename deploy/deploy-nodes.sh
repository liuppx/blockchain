#!/usr/bin/env bash
# 在服务器上从**社区 main** 拉源码、编译、部署知行图测试网节点。
#
#   deploy/deploy-nodes.sh                 # 拉社区 main → 编译 → 部署 → 自检
#   deploy/deploy-nodes.sh --check         # 只看远端状态,不改动
#   deploy/deploy-nodes.sh --ref <commit>  # 指定社区 main 上的某个提交(默认最新 main)
#
# 流程(约定:代码先合并到社区 main,发布物由服务器自己编译):
#   1. /root/code/blockchain 拉取社区仓库(yeying-community/blockchain)的 main;
#   2. 在服务器上 cargo build --release(服务器是 x86_64,直接编,不需要交叉编译);
#   3. 把二进制与脚本装进 /opt/deploy/<包名>/;
#   4. 迁移配置与密钥(不动链数据)、停旧版、启新版;
#   5. 健康检查 → finalize → 切软链接 → 冒烟 → 自检。
#
# 不再从本地打包、拷贝二进制过来:发布物与社区 main 的内容天然一致。
# 前置:服务器上要装好 Rust 工具链(deploy/install-rust.sh)。
#
# 环境变量:
#   HOST        节点机器(默认 root@221.130.30.75,端口 10022)
#   REPO        社区仓库(默认 https://github.com/yeying-community/blockchain.git)
#   CODE_DIR    服务器上的源码目录(默认 /root/code/blockchain)
#   VERSION     版本号(默认取社区 main 最近一次 tag,没有则 0.1.0)
#   REF         要发布的提交(默认社区 main 的最新提交)
#   SKIP_BUILD  只部署不编译(用于服务器上已经编好、只想重装的情况)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HOST="${HOST:-root@221.130.30.75}"
REPO="${REPO:-https://github.com/yeying-community/blockchain.git}"
CODE_DIR="${CODE_DIR:-/root/code/blockchain}"
VERSION="${VERSION:-}"
REF="${REF:-}"
CHECK_ONLY=0
SKIP_BUILD=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1
[ "${1:-}" = "--skip-build" ] && SKIP_BUILD=1

SSH=(ssh -p 10022 "$HOST")
DEPLOY_ROOT=/opt/deploy
UNIT=zhixing-testnet-service

info() { printf '\n== %s\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

if [ "$CHECK_ONLY" = 1 ]; then
  info "远端状态($HOST)"
  "${SSH[@]}" "readlink -f $DEPLOY_ROOT/zhixing-testnet; $DEPLOY_ROOT/zhixing-testnet/scripts/starter.sh status; curl -s http://127.0.0.1:9221/height"
  exit 0
fi

# ---------------------------------------------------------------- 拉社区 main
info "拉取社区 main: $REPO"
"${SSH[@]}" "set -e
  if [ ! -d '$CODE_DIR/.git' ]; then
    mkdir -p \$(dirname '$CODE_DIR')
    git clone '$REPO' '$CODE_DIR'
  fi
  cd '$CODE_DIR'
  git remote set-url origin '$REPO'
  git fetch --prune origin main
  # 工作区不干净就拒绝:不覆盖服务器上的改动(那是人改的,要先看清楚)
  if [ -n \"\$(git status --porcelain)\" ]; then
    echo '工作区有未提交改动,拒绝拉取;先在服务器上处理:' >&2
    git status --short >&2
    exit 1
  fi
  git checkout -q main 2>/dev/null || git checkout -q -B main origin/main
  git reset --hard origin/main
  git clean -fdq
  echo \"  HEAD = \$(git rev-parse --short HEAD)\"
  echo \"  提交标题:\$(git log -1 --pretty=%s)\""

# 待发布的提交
TARGET="${REF:-origin/main}"
COMMIT="$("${SSH[@]}" "cd '$CODE_DIR' && git rev-parse --verify '${TARGET}^{commit}'" | tr -dc 0-9a-f | head -40)"
[ -n "$COMMIT" ] || fail "拿不到目标提交 $TARGET"
SHORT="${COMMIT:0:7}"
info "待发布提交 $COMMIT($SHORT)"

# 必须是**社区 main 上**的提交(约定:代码先合并到社区 main 才发布)
ON_MAIN="$("${SSH[@]}" "cd '$CODE_DIR' && git merge-base --is-ancestor '$COMMIT' origin/main && echo yes || echo no" | tr -dc 'a-z' | head -3)"
[ "$ON_MAIN" = "yes" ] || fail "提交 $COMMIT 不在社区 main 上——先把代码合并到社区 main 再发布"
echo "  ✓ 已在社区 main 上"

if [ -z "$VERSION" ]; then
  VERSION="$("${SSH[@]}" "cd '$CODE_DIR' && (git describe --tags --abbrev=0 2>/dev/null || echo 0.1.0)" | tr -dc '0-9.' | head -20)"
fi
PKG_ROOT="${UNIT}-v${VERSION}-${SHORT}"
TGT="$DEPLOY_ROOT/$PKG_ROOT"

# ---------------------------------------------------------------- 服务器上编译
if [ "$SKIP_BUILD" = 0 ]; then
  info "在服务器上编译($CODE_DIR,cargo build --release)"
  "${SSH[@]}" "set -e
    cd '$CODE_DIR'
    # 非交互 ssh 不读 ~/.profile,rustup 装的 cargo 不在 PATH 上:显式加载
    [ -f \"\$HOME/.cargo/env\" ] && . \"\$HOME/.cargo/env\"
    command -v cargo >/dev/null || { echo '服务器上没有 cargo:先跑 deploy/install-rust.sh' >&2; exit 1; }
    (cd node && cargo build --release --locked)
    test -x node/target/release/node
    echo \"  编好的二进制:\$(ls -l node/target/release/node | awk '{print \$5}') 字节\"
    mkdir -p '$TGT/bin' '$TGT/scripts' '$TGT/tools/explorer'
    install -m 0755 node/target/release/node '$TGT/bin/node'
    cp scripts/starter.sh scripts/health-check.sh scripts/test.sh scripts/upgrade.sh '$TGT/scripts/'
    rm -rf '$TGT/tools/explorer'
    cp -R tools/explorer '$TGT/tools/explorer'
    rm -rf '$TGT/tools/explorer/node_modules' '$TGT/tools/explorer/reference' '$TGT/tools/explorer/test'
    mkdir -p '$TGT/testnet' '$TGT/docs'
    cp testnet/gen.sh '$TGT/testnet/'
    cp docs/pilot/README.md '$TGT/docs/' 2>/dev/null || true
    echo '  装好了'"
fi

info "升级:现有版本 → $PKG_ROOT"
CUR="$("${SSH[@]}" "readlink -f $DEPLOY_ROOT/zhixing-testnet")"
echo "  当前:$CUR"

"${SSH[@]}" "set -e
  bash '$TGT/scripts/upgrade.sh' prepare '$CUR' '$TGT'
  bash '$CUR/scripts/starter.sh' stop
  bash '$TGT/scripts/upgrade.sh' copy-config '$CUR' '$TGT'
  bash '$TGT/scripts/upgrade.sh' post-copy '$CUR' '$TGT'
  bash '$TGT/scripts/starter.sh' start"

info "等节点追上历史(重放 1–2 分钟)"
"${SSH[@]}" "bash '$TGT/scripts/health-check.sh' --level all --retries 40 --interval 6" \
  || fail "健康检查没过(节点可能还在重放,过一会儿再跑 deploy-nodes.sh --check 看)"

"${SSH[@]}" "set -e
  bash '$TGT/scripts/upgrade.sh' finalize '$CUR' '$TGT'
  ln -sfn '$PKG_ROOT' $DEPLOY_ROOT/zhixing-testnet
  bash $DEPLOY_ROOT/zhixing-testnet/scripts/test.sh --suite smoke"

info "自检"
"${SSH[@]}" "set -e
  echo \"版本: \$(readlink -f $DEPLOY_ROOT/zhixing-testnet)\"
  $DEPLOY_ROOT/zhixing-testnet/scripts/starter.sh status
  echo -n '高度: '; curl -s http://127.0.0.1:9221/height; echo
  CODE=\$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary '' http://127.0.0.1:9221/key-rotation)
  echo \"key-rotation 端点: HTTP \$CODE\"
  test \"\$CODE\" != '404' || { echo '换密钥端点不存在' >&2; exit 1; }
  echo -n '高度(再读一次): '; curl -s http://127.0.0.1:9221/height; echo"

echo
echo "== 完成:节点已升级到 $PKG_ROOT(社区 main $SHORT,服务器上编译)"
