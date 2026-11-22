#!/usr/bin/env bash
# 重新生成 public/vendor/three/three-map.min.js(认知地图立体视图用的 three.js 子集)。
#
#   bash tools/explorer/scripts/build-three.sh            # 用固定版本重建
#   THREE_VERSION=0.188.0 bash tools/explorer/scripts/build-three.sh   # 升级
#
# 只在开发机上跑:从 npm 官方包取 three 源码,用 esbuild 只打包 scripts/three-map.entry.js 列出的部件。
# 浏览器服务本身仍然零依赖——这里产出的是一个随网页分发的静态文件(MIT 许可,LICENSE 一并复制)。
# 产物是确定的:同一版本、同一入口,重建结果逐字节相同,可以用来核对仓库里的文件。
set -euo pipefail
THREE_VERSION="${THREE_VERSION:-0.187.0}"
ESBUILD_VERSION="${ESBUILD_VERSION:-0.25.12}"
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/../public/vendor/three"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cd "$WORK"
npm init -y >/dev/null
# 一次装齐(分两次装时,后一次会把前一次的 --no-save 包清掉)
npm install --no-save --no-audit --no-fund --loglevel=error "three@${THREE_VERSION}" "esbuild@${ESBUILD_VERSION}" >/dev/null
cp "$HERE/three-map.entry.js" entry.js

mkdir -p "$OUT"
./node_modules/.bin/esbuild entry.js --bundle --format=esm --minify --target=es2020 \
  --legal-comments=none \
  --banner:js="/* three.js r${THREE_VERSION#0.} (MIT, https://threejs.org) — 认知地图用到的子集,由 tools/explorer/scripts/build-three.sh 生成,请勿手改 */" \
  --outfile="$OUT/three-map.min.js" --log-level=warning
cp node_modules/three/LICENSE "$OUT/LICENSE"
echo "three@${THREE_VERSION} esbuild@${ESBUILD_VERSION}" > "$OUT/VERSION"
ls -l "$OUT"
printf 'gzip 后:'; gzip -9c "$OUT/three-map.min.js" | wc -c
