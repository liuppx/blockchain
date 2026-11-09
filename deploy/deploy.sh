#!/usr/bin/env bash
# 知行图测试网 · 单机部署(4 个验证节点 + 浏览器 + nginx TLS),目标:blockchain.yeying.pub
#
#   deploy/deploy.sh                 # 同步程序/脚本/配置并重启服务(保留链数据与密钥)
#   deploy/deploy.sh --genesis       # 首次开链:在服务器上生成全新验证人密钥与创世(销毁旧链!)
#
# 环境变量:HOST(默认 root@122.9.144.27)、BIN(linux x86_64 的 node 二进制)
#
# 设计要点:
# - 验证人密钥只在服务器上生成,从不经过本机;本机也不读取服务器上的密码与令牌。
# - 成员只给公钥(testnet/members.toml),私钥由成员自己持有。
# - 节点 RPC/指标/P2P 全部只监听 127.0.0.1,对外只有 nginx 的 443(/rpc/ 与浏览器)。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
HOST="${HOST:-root@122.9.144.27}"
BIN="${BIN:-$ROOT/node/target-linux/x86_64-unknown-linux-gnu/release/node}"
DEST=/opt/zhixing
GENESIS=0
case "${1:-}" in
  --genesis) GENESIS=1 ;;
  "") ;;
  *) echo "usage: $0 [--genesis]" >&2; exit 2 ;;
esac

[ -x "$BIN" ] || { echo "missing linux binary: $BIN (see deploy/README.md 构建)" >&2; exit 1; }
file "$BIN" | grep -q 'x86-64' || { echo "$BIN is not an x86-64 binary" >&2; exit 1; }
[ "$GENESIS" = 0 ] || [ -f "$ROOT/testnet/members.toml" ] || {
  echo "testnet/members.toml missing (needed for --genesis)" >&2; exit 1; }

echo "== sync → $HOST:$DEST"
ssh "$HOST" "mkdir -p $DEST/node/target/release $DEST/testnet $DEST/tools $DEST/docs $DEST/deploy"
rsync -az "$BIN" "$HOST:$DEST/node/target/release/node.new"
rsync -az --delete --exclude out/ --exclude 'members.toml' "$ROOT/testnet/" "$HOST:$DEST/testnet/"
[ -f "$ROOT/testnet/members.toml" ] && rsync -az "$ROOT/testnet/members.toml" "$HOST:$DEST/testnet/members.toml"
rsync -az --delete --exclude node_modules "$ROOT/tools/" "$HOST:$DEST/tools/"
rsync -az --delete "$ROOT/docs/pilot/" "$HOST:$DEST/docs/pilot/"
rsync -az --delete "$HERE/" "$HOST:$DEST/deploy/"

echo "== install on server"
ssh "$HOST" GENESIS="$GENESIS" DEST="$DEST" bash -s <<'REMOTE'
set -euo pipefail
id zhixing >/dev/null 2>&1 || useradd --system --home-dir "$DEST" --shell /usr/sbin/nologin zhixing

UNITS="zhixing-explorer $(for i in 21 22 23 24; do printf 'zhixing-node@%s ' $i; done)"
systemctl stop $UNITS 2>/dev/null || true
mv "$DEST/node/target/release/node.new" "$DEST/node/target/release/node"
chmod 755 "$DEST/node/target/release/node"

if [ "$GENESIS" = 1 ]; then
  echo "-- fresh genesis (validator keys generated here, never leave the server)"
  chown -R root:root "$DEST/testnet"
  "$DEST/testnet/gen.sh" --members "$DEST/testnet/members.toml" --force
fi
[ -f "$DEST/testnet/out/password" ] || { echo "no chain yet — rerun with --genesis" >&2; exit 1; }

install -d -m 700 /etc/zhixing
umask 077
printf 'ZHIXING_KEYSTORE_PASSWORD=%s\n' "$(cat "$DEST/testnet/out/password")" > /etc/zhixing/env
chmod 600 /etc/zhixing/env
chown -R zhixing:zhixing "$DEST/testnet/out"

install -m 644 "$DEST/deploy/systemd/zhixing-node@.service" /etc/systemd/system/
install -m 644 "$DEST/deploy/systemd/zhixing-explorer.service" /etc/systemd/system/
systemctl daemon-reload

conf=/etc/nginx/conf.d/blockchain.conf
if [ -f "$conf" ] && ! grep -q '知行图测试网' "$conf"; then
  cp "$conf" "/root/blockchain.conf.pre-zhixing.$(date +%Y%m%d%H%M%S)"
fi
# conf.d 里的 *.bak 不会被 include(只匹配 *.conf),保留原样
install -m 644 "$DEST/deploy/nginx/blockchain.conf" "$conf"
nginx -t
systemctl reload nginx

systemctl enable --now $UNITS
sleep 4
systemctl --no-pager --lines=0 status $UNITS | grep -E '●|Active:' || true
echo "-- height: $(curl -fsS http://127.0.0.1:9221/height || echo '?')"
echo "-- genesis identity:"; sed 's/^/   /' "$DEST/testnet/out/genesis-identity.txt"
REMOTE

echo "== public smoke"
sleep 2
curl -fsS "https://blockchain.yeying.pub/rpc/height" && echo "  ← https /rpc/height"
curl -fsS -o /dev/null -w '%{http_code}  ← https / (explorer)\n' "https://blockchain.yeying.pub/"
echo "RPC 写令牌在服务器上:$DEST/testnet/out/tokens.env(RPC_TOKEN),请勿贴到公开渠道"
