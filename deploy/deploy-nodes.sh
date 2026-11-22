#!/usr/bin/env bash
# 升级测试网的验证节点(4 个,都在新机 221.130.30.75)。
#
#   deploy/deploy-nodes.sh                 # 打包 → 上传 → 解压 → 升级 → 自检
#   deploy/deploy-nodes.sh --check         # 只看远端状态,不改动
#
# 流程照 deploy/README.md 的「升级(手动执行升级器的流程)」:
#   打包(本地,只从已提交代码)→ 上传 → 校验 sha256 → 解压 → prepare → 停旧版
#   → copy-config → post-copy → 启新版 → 健康检查 → finalize → 切软链接 → 冒烟。
#
# 设计要点:
# - 打包只从**已提交**的代码构建(隔离 worktree),不推送、不建 tag;
# - 链数据与密钥都不动:config/ 由升级器从旧版本目录迁移,数据仍在 /data/zhixing-testnet/;
# - 每一步都有自检:校验和、4 个节点都在跑、高度在涨、冒烟通过;
# - 回退:同一套步骤反过来(旧版本目录还在,finalize 之前都可以退)。
#
# 环境变量:
#   HOST       节点机器(默认 root@221.130.30.75,端口 10022)
#   VERSION    版本号(默认 0.1.6)
#   REF        要发布的提交(默认当前 HEAD;必须是已提交的)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HOST="${HOST:-root@221.130.30.75}"
VERSION="${VERSION:-0.1.6}"
REF="${REF:-HEAD}"
CHECK_ONLY=0
[ "${1:-}" = "--check" ] && CHECK_ONLY=1

SSH=(ssh -p 10022 "$HOST")
PKG_DIR=/tmp/kdg-pkg
PKG_ROOT="zhixing-testnet-service-v${VERSION}"

info() { printf '\n== %s\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }

if [ "$CHECK_ONLY" = 1 ]; then
  info "远端状态($HOST)"
  "${SSH[@]}" "readlink -f /opt/deploy/zhixing-testnet; /opt/deploy/zhixing-testnet/scripts/starter.sh status; curl -s http://127.0.0.1:9221/height"
  exit 0
fi

cd "$ROOT"
COMMIT="$(git rev-parse --verify "${REF}^{commit}")" || fail "$REF 不是一个提交"
SHORT="${COMMIT:0:7}"
PKG_NAME="${PKG_ROOT}-${SHORT}.tar.gz"
info "打包 $PKG_NAME(源码 $COMMIT)"
rm -rf "$PKG_DIR"
bash "$ROOT/scripts/package.sh" --ref "$COMMIT" --source-commit "$COMMIT" \
  --version "$VERSION" --output-dir "$PKG_DIR" >/dev/null
[ -f "$PKG_DIR/$PKG_NAME" ] || fail "打包没产出 $PKG_NAME"

info "上传 → $HOST:/opt/package/"
rsync -az "$PKG_DIR/" -e "ssh -p 10022" "$HOST:/opt/package/"

info "远端校验并解压"
"${SSH[@]}" "set -e
  cd /opt/package
  sha256sum -c '${PKG_NAME}.sha256sum'
  tar -xzf '${PKG_NAME}' -C /opt/deploy/
  test -d '/opt/deploy/${PKG_ROOT}-${SHORT}'
  echo 'extracted'"

CUR="$("${SSH[@]}" "readlink -f /opt/deploy/zhixing-testnet")"
TGT="/opt/deploy/${PKG_ROOT}-${SHORT}"
info "升级:$CUR → $TGT"

"${SSH[@]}" "set -e
  bash '${TGT}/scripts/upgrade.sh' prepare '$CUR' '${TGT}'
  bash '${CUR}/scripts/starter.sh' stop
  bash '${TGT}/scripts/upgrade.sh' copy-config '$CUR' '${TGT}'
  bash '${TGT}/scripts/upgrade.sh' post-copy '$CUR' '${TGT}'
  bash '${TGT}/scripts/starter.sh' start"

info "等节点追上历史(重放 1–2 分钟)"
"${SSH[@]}" "bash '${TGT}/scripts/health-check.sh' --level all --retries 40 --interval 6" \
  || fail "健康检查没过(节点可能还在重放,过一会儿再跑 deploy-nodes.sh --check 看)"

"${SSH[@]}" "set -e
  bash '${TGT}/scripts/upgrade.sh' finalize '$CUR' '${TGT}'
  ln -sfn '${PKG_ROOT}-${SHORT}' /opt/deploy/zhixing-testnet
  bash /opt/deploy/zhixing-testnet/scripts/test.sh --suite smoke"

info "自检:4 个节点、换密钥端点、当前版本"
"${SSH[@]}" "set -e
  echo \"版本: \$(readlink -f /opt/deploy/zhixing-testnet)\"
  /opt/deploy/zhixing-testnet/scripts/starter.sh status
  echo -n '高度: '; curl -s http://127.0.0.1:9221/height
  echo
  # 换密钥端点存在:空请求应返回 400(能解码才 400),不是 404
  CODE=\$(curl -s -o /dev/null -w '%{http_code}' -X POST --data-binary '' http://127.0.0.1:9221/key-rotation)
  echo \"key-rotation 端点: HTTP \$CODE\"
  test \"\$CODE\" != '404' || { echo '换密钥端点不存在' >&2; exit 1; }
  echo -n '高度(再读一次): '; curl -s http://127.0.0.1:9221/height; echo"

echo
echo "== 完成:节点已升级到 $TGT"
echo "下一步(换 alice / bob 的密钥):"
echo "  testnet/rotate-key.sh --account 1 --old-key <alice.seed>"
echo "  testnet/rotate-key.sh --account 2 --old-key <bob.seed>"
