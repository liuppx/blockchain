#!/usr/bin/env bash
# 在节点服务器上装 Rust 工具链(社区 main 编译所需)。
#
#   deploy/install-rust.sh              # 装到 /usr/local(系统级)
#   deploy/install-rust.sh --check      # 只看装没装、什么版本
#
# 一次装好,以后发版都在服务器上编译(deploy-nodes.sh)。
# 版本用 rustup 的 stable;仓库里没有 rust-toolchain 文件,不锁版本。
# 交叉编译那套(本地 Docker 编 x86_64)用不上了——服务器本身就是 x86_64。
set -euo pipefail

HOST="${HOST:-root@221.130.30.75}"
SSH=(ssh -p 10022 "$HOST")

if [ "${1:-}" = "--check" ]; then
  "${SSH[@]}" "command -v cargo >/dev/null && { cargo --version; rustc --version; } || echo '还没装'"
  exit 0
fi

echo "== 在 $HOST 上装 Rust"
"${SSH[@]}" "set -e
  if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
    . \"\$HOME/.cargo/env\"
  fi
  export PATH=\"\$HOME/.cargo/bin:\$PATH\"
  echo \"  cargo: \$(cargo --version)\"
  echo \"  rustc: \$(rustc --version)\"
  # 链接器与构建依赖(Rust 需要一个 C 链接器)
  command -v cc >/dev/null || { apt-get update -qq && apt-get install -y -qq build-essential >/dev/null; }
  echo '  ok'"
