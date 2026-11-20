#!/usr/bin/env bash
# 打包知行图测试网节点(社区规范 PACKAGING.md v1)。
#
#   scripts/package.sh --ref <commit> --source-commit <commit> --version <major.minor.patch> --output-dir <dir>
#
# 产出:<unit-id>-v<version>-<commit7>.tar.gz 与 release-manifest.json。
# 规则:只从已提交代码构建;不创建/推送 git ref;不部署;不清理调用方传入的目录。
#
# 包内含:发布二进制、节点配置模板、scripts/{starter,health-check,test,upgrade}.sh、
# 浏览器服务(tools/explorer)、testnet 生成脚本。不含任何密钥、创世或链数据。
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT_ID="zhixing-testnet-service"

REF=""
SOURCE_COMMIT=""
VERSION=""
OUTPUT_DIR=""

info() { printf '%s\n' "$*" >&2; }
fail() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }

usage() {
  cat >&2 <<'EOF'
Usage: scripts/package.sh --ref <commit> --source-commit <commit> --version <major.minor.patch> --output-dir <dir>
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ref) REF="${2:-}"; shift 2 ;;
    --source-commit) SOURCE_COMMIT="${2:-}"; shift 2 ;;
    --version) VERSION="${2:-}"; shift 2 ;;
    --output-dir) OUTPUT_DIR="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) fail "unknown argument: $1" ;;
  esac
done
[[ -n "$REF" ]] || { usage; fail "--ref is required"; }
[[ -n "$SOURCE_COMMIT" ]] || { usage; fail "--source-commit is required"; }
[[ -n "$VERSION" ]] || { usage; fail "--version is required"; }
[[ -n "$OUTPUT_DIR" ]] || { usage; fail "--output-dir is required"; }
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "--version must be major.minor.patch"
[[ "$OUTPUT_DIR" != "/" && "$OUTPUT_DIR" != *"/.." ]] || fail "--output-dir must not contain .."

command -v git >/dev/null 2>&1 || fail "git is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"
if command -v sha256sum >/dev/null 2>&1; then sha_command=sha256sum
elif command -v shasum >/dev/null 2>&1; then sha_command=shasum
else fail "sha256sum or shasum is required"; fi

cd "$ROOT_DIR"
git rev-parse --verify --quiet "${REF}^{commit}" >/dev/null || fail "--ref is not a commit: $REF"
git rev-parse --verify --quiet "${SOURCE_COMMIT}^{commit}" >/dev/null || fail "--source-commit is not a commit: $SOURCE_COMMIT"
SOURCE_SHA="$(git rev-parse "${SOURCE_COMMIT}^{commit}")"
git merge-base --is-ancestor "$SOURCE_SHA" "${REF}^{commit}" 2>/dev/null \
  || fail "--source-commit $SOURCE_SHA is not an ancestor of --ref"
SHORT_COMMIT="${SOURCE_SHA:0:7}"
PACKAGE_ROOT="${UNIT_ID}-v${VERSION}-${SHORT_COMMIT}"
PACKAGE_NAME="${PACKAGE_ROOT}.tar.gz"

WORKTREE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/zhixing-package.XXXXXX")"
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/zhixing-stage.XXXXXX")"
cleanup() {
  git worktree remove --force "$WORKTREE_DIR" >/dev/null 2>&1 || true
  rm -rf "$STAGE_DIR"
}
trap cleanup EXIT

info "== building in an isolated worktree at $REF"
git worktree add --detach "$WORKTREE_DIR" "${REF}^{commit}" >/dev/null

# 目标平台的发布二进制。构建机可能不是 x86_64(如本机是 arm64),用交叉编译容器。
if [[ "$(uname -m)" == "x86_64" ]]; then
  (cd "$WORKTREE_DIR/node" && cargo build --release --locked)
  BIN_SRC="$WORKTREE_DIR/node/target/release/node"
else
  command -v docker >/dev/null 2>&1 || fail "need docker to cross-compile for x86_64"
  docker run --rm -v "$WORKTREE_DIR:/src" -v zhixing-cargo-registry:/usr/local/cargo/registry \
    -w /src/node rust:1.90-bookworm sh -c '
      set -e
      apt-get update -qq >/dev/null
      apt-get install -y -qq gcc-x86-64-linux-gnu libc6-dev-amd64-cross >/dev/null
      command -v x86_64-linux-gnu-gcc >/dev/null
      rustup target add x86_64-unknown-linux-gnu >/dev/null 2>&1
      export CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
      export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
      cargo build --release --locked --target x86_64-unknown-linux-gnu --target-dir /src/node/target-linux'
  BIN_SRC="$WORKTREE_DIR/node/target-linux/x86_64-unknown-linux-gnu/release/node"
fi
[[ -x "$BIN_SRC" ]] || fail "release binary not found: $BIN_SRC"

PACKAGE_DIR="$STAGE_DIR/$PACKAGE_ROOT"
mkdir -p "$PACKAGE_DIR"/{bin,scripts,config,tools/explorer,testnet,docs}
cp "$BIN_SRC" "$PACKAGE_DIR/bin/node"
cp -R "$WORKTREE_DIR/tools/explorer/." "$PACKAGE_DIR/tools/explorer/"
rm -rf "$PACKAGE_DIR/tools/explorer/node_modules"
# reference/(参考签名器)与 test/ 只给测试用,不进安装包:部署出去的浏览器里不带任何私钥代码
rm -rf "$PACKAGE_DIR/tools/explorer/reference" "$PACKAGE_DIR/tools/explorer/test"
cp "$WORKTREE_DIR/scripts/starter.sh" "$WORKTREE_DIR/scripts/health-check.sh" \
   "$WORKTREE_DIR/scripts/test.sh" "$WORKTREE_DIR/scripts/upgrade.sh" "$PACKAGE_DIR/scripts/"
cp "$WORKTREE_DIR/testnet/gen.sh" "$PACKAGE_DIR/testnet/"
cp "$WORKTREE_DIR/docs/pilot/README.md" "$PACKAGE_DIR/docs/" 2>/dev/null || true
cat > "$PACKAGE_DIR/config/README.md" <<'EOF'
# 配置目录

部署时把以下内容放在这里(不属于安装包,含密钥,权限 700):

- `genesis.toml` — 创世(含成员名单与领域)
- `node{n}.toml` — 每个节点的配置
- `keys/` — 验证人密钥库(600)
- `password` — 密钥库密码(600)
- `tokens.env` — RPC 写令牌与指标令牌(600)
EOF
cat > "$PACKAGE_DIR/README.md" <<EOF
# 知行图测试网节点(安装包 ${PACKAGE_ROOT})

- \`bin/node\` — 节点程序
- \`scripts/starter.sh\` — 启停(\`start|stop|restart\`)
- \`scripts/health-check.sh\` — 健康检查(\`--level liveness|readiness|dependency|all\`)
- \`scripts/test.sh\` — 测试(\`--suite smoke\`)
- \`scripts/upgrade.sh\` — 升级阶段(\`prepare|copy-config|post-copy|finalize\`)
- \`config/\` — 配置与密钥(安装后放,不在包内)
- 链数据目录由 \`ZHIXING_DATA_DIR\` 指定(默认 \`/data/zhixing-testnet\`)

版本 ${VERSION},源码 ${SOURCE_SHA}。
EOF

tar -czf "$STAGE_DIR/$PACKAGE_NAME" -C "$STAGE_DIR" "$PACKAGE_ROOT"
CHECKSUM="$("$sha_command" "$STAGE_DIR/$PACKAGE_NAME" | awk '{print $1}')"
mkdir -p "$OUTPUT_DIR"
cp "$STAGE_DIR/$PACKAGE_NAME" "$OUTPUT_DIR/"
printf '%s  %s\n' "$CHECKSUM" "$PACKAGE_NAME" > "$OUTPUT_DIR/$PACKAGE_NAME.sha256sum"

cat > "$OUTPUT_DIR/release-manifest.json" <<EOF
{
  "schema_version": 1,
  "repository": "blockchain",
  "source_commit": "$SOURCE_SHA",
  "artifacts": [
    {
      "unit_id": "$UNIT_ID",
      "type": "service",
      "version": "$VERSION",
      "package": "$PACKAGE_NAME",
      "sha256": "$CHECKSUM",
      "package_root": "$PACKAGE_ROOT",
      "scripts": {
        "starter": "scripts/starter.sh",
        "health_check": "scripts/health-check.sh",
        "test": "scripts/test.sh"
      },
      "data_dir": "/data/zhixing-testnet",
      "service_target": {"module":"zhixing-testnet", "path":"/opt/deploy/zhixing-testnet"}
    }
  ]
}
EOF

info "package : $OUTPUT_DIR/$PACKAGE_NAME"
info "sha256  : $CHECKSUM"
info "manifest: $OUTPUT_DIR/release-manifest.json"
