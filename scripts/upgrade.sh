#!/usr/bin/env bash
# 知行图测试网:升级阶段钩子(社区规范 UPGRADE.md「模块升级行为」)。
#
#   scripts/upgrade.sh prepare|copy-config|post-copy|finalize <current-dir> <target-dir>
#
# 升级器的顺序:prepare → 当前版本 starter.sh stop(确认退出)→ copy-config → post-copy
#               → 目标版本 starter.sh → health-check.sh --level all → finalize → 切换软链接。
#
# 本项目:
# - 链数据在 /data/zhixing-testnet(节点配置里的 data_dir),不随版本目录移动;
# - copy-config 把 config/(创世、节点配置、验证人密钥库、密码、令牌)带到目标版本,
#   并把节点配置里指向旧版本目录的路径(创世、密钥库)改写到目标版本目录;
# - 不打印任何密钥、密码或令牌。
set -euo pipefail

PHASE="${1:-}"
CURRENT="${2:-}"
TARGET="${3:-}"

log() { printf '[%s] upgrade.sh %s: %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$PHASE" "$*"; }
fail() { log "ERROR: $*" >&2; exit 1; }

usage() { echo "Usage: $0 prepare|copy-config|post-copy|finalize <current-dir> <target-dir>" >&2; }

case "$PHASE" in
  prepare|copy-config|post-copy|finalize) ;;
  -h|--help) usage; exit 0 ;;
  *) usage; exit 2 ;;
esac
[[ -n "$CURRENT" && -n "$TARGET" ]] || { usage; exit 2; }
[[ -d "$TARGET" ]] || fail "target dir not found: $TARGET"

case "$PHASE" in
  prepare)
    [[ -d "$CURRENT/config" ]] || fail "current version has no config/: $CURRENT/config"
    [[ -x "$TARGET/bin/node" ]] || fail "target version has no executable bin/node"
    for f in genesis.toml password; do [[ -f "$CURRENT/config/$f" ]] || fail "missing $CURRENT/config/$f"; done
    log "ok: current=$(basename "$CURRENT") target=$(basename "$TARGET")"
    ;;

  copy-config)
    # 只在旧服务已确认停止后被调用(升级器保证)
    [[ -d "$CURRENT/config" ]] || fail "current version has no config/"
    mkdir -p "$TARGET/config"
    # 包里的 config/ 只有 README;用当前版本的配置覆盖,保持权限
    cp -a "$CURRENT/config/." "$TARGET/config/"
    # 节点配置中的创世与密钥库路径指向当前版本目录 → 改写为目标版本目录(数据目录不变)
    cur_abs="$(cd "$CURRENT" && pwd -P)"
    tgt_abs="$(cd "$TARGET" && pwd -P)"
    n=0
    for cfg in "$TARGET"/config/node*.toml; do
      [[ -f "$cfg" ]] || continue
      sed -i -e "s#${cur_abs}/config/#${tgt_abs}/config/#g" "$cfg"
      n=$((n+1))
    done
    chmod 700 "$TARGET/config"
    [[ -d "$TARGET/config/keys" ]] && chmod 700 "$TARGET/config/keys" && chmod 600 "$TARGET"/config/keys/* 2>/dev/null || true
    for f in password tokens.env; do [[ -f "$TARGET/config/$f" ]] && chmod 600 "$TARGET/config/$f"; done
    # 自检:改写后不应再引用旧版本目录
    if grep -l "${cur_abs}/" "$TARGET"/config/node*.toml >/dev/null 2>&1; then
      fail "node configs still reference the old version dir"
    fi
    log "copied config/ and rewrote $n node config(s) to $(basename "$TARGET")"
    ;;

  post-copy)
    log "nothing to do (no static assets in this unit)"
    ;;

  finalize)
    log "ok"
    ;;
esac
