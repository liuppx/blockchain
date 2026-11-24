#!/usr/bin/env bash
# 在测试网上激活一次按高度生效的协议升级(默认:知识经济 v1)。
#
#   deploy/activate-upgrade.sh --height H                  # 默认节奏(结算 1 天、锁定 180 天、追加 3 天)
#   deploy/activate-upgrade.sh --height H --dry-run        # 只检查、只打印,不改任何东西
#   deploy/activate-upgrade.sh --check                     # 看远端当前的时刻表
#
# 做的事:
#   1. 前置检查:4 个节点都在跑、高度在涨;H 离当前高度足够远(默认至少 1 小时),来得及重启;
#      远端当前版本认识这个升级名(`node genesis-hash` 对新创世不报错);
#   2. 备份创世:config/genesis.toml → config/genesis.toml.bak-<时间>;
#   3. 在创世末尾追加 `[[upgrades]]`;**创世哈希不变**(脚本核对:改前改后 genesis_hash 相同);
#   4. 每个节点配置上跑 `genesis-hash`,4 个节点的 `upgrades` 行必须完全一致;
#   5. 依次重启(starter.sh restart),等全部追上、高度在涨;
#   6. 打印 /economy:应显示 active=false、activation_height=H(到 H 才生效)。
#
# 回退(H 之前随时可以):把备份的 genesis.toml 换回去,再 starter.sh restart。
# H 之后回退要所有节点一起停在同一高度换回——不建议;到时以"再排一次升级"修正。
#
# 不动:链数据、密钥、节点版本(版本由 deploy-nodes.sh 管)。
set -euo pipefail

HOST="${HOST:-root@221.130.30.75}"
SSH=(ssh -p 10022 "$HOST")
DEPLOY=/opt/deploy/zhixing-testnet
RPC_LOCAL=http://127.0.0.1:9221
NAME="knowledge-economy-v1"
HEIGHT=""
MIN_LEAD="${MIN_LEAD:-3600}"   # H 至少比当前高度高这么多块(约 1 小时)
DRY=0
CHECK=0
while [ $# -gt 0 ]; do
  case "$1" in
    --height) HEIGHT="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --check) CHECK=1; shift ;;
    *) echo "未知参数 $1" >&2; exit 2 ;;
  esac
done

info() { printf '\n== %s\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }
remote() { "${SSH[@]}" "$@"; }

if [ "$CHECK" = 1 ]; then
  info "远端时刻表"
  remote "cd $DEPLOY && for n in 21 22 23 24; do printf 'n%s: ' \$n; ./bin/node genesis-hash --config config/node\$n.toml | tail -1; done; curl -s $RPC_LOCAL/economy | head -1"
  exit 0
fi

[ -n "$HEIGHT" ] || fail "需要 --height H"
[[ "$HEIGHT" =~ ^[0-9]+$ ]] || fail "--height 应为整数"

info "前置检查"
CUR="$(remote "curl -s --max-time 5 $RPC_LOCAL/height" | tr -dc 0-9)"
[ -n "$CUR" ] || fail "读不到远端高度"
echo "  当前高度 $CUR,激活高度 $HEIGHT(还有 $((HEIGHT - CUR)) 块,约 $(( (HEIGHT - CUR) / 3600 )) 小时)"
[ "$HEIGHT" -gt $((CUR + MIN_LEAD)) ] || fail "激活高度离现在太近(至少要领先 $MIN_LEAD 块),来不及安全重启"
remote "cd $DEPLOY && scripts/starter.sh status" | sed 's/^/  /'
remote "grep -q '^\\[\\[upgrades\\]\\]' $DEPLOY/config/genesis.toml" \
  && fail "远端创世里已经有 [[upgrades]] 了;先用 --check 看清楚,不要重复追加"

# 远端版本必须**真的实现了**这个升级,而不只是认识它的名字。
# 教训:0.1.7 只有激活机制、没有经济规则,却认识 knowledge-economy-v1 这个名字——
# 用它排期,到了 H 节点会"激活"一套空规则,之后换新版本也接不上(状态根不同)。
# 所以要求远端节点已经提供 /economy,且带结算字段(S3 起才有)。
info "远端版本是否实现了知识经济(要先用 deploy-nodes.sh 部署新版本)"
VER_DIR="$(remote "readlink -f $DEPLOY")"
echo "  远端版本:$VER_DIR"
ECON="$(remote "curl -s --max-time 5 -H 'Accept: application/json' $RPC_LOCAL/economy")"
case "$ECON" in
  *'"period"'*) echo "  ✓ /economy 可用且带结算字段" ;;
  *) fail "远端节点没有实现知识经济(/economy 不可用或缺结算字段):先部署新版本。响应:${ECON:0:120}" ;;
esac
if [ -n "${EXPECT_COMMIT:-}" ]; then
  case "$VER_DIR" in *"$EXPECT_COMMIT"*) echo "  ✓ 版本是预期的提交 $EXPECT_COMMIT" ;;
    *) fail "远端版本不是预期的提交 $EXPECT_COMMIT" ;; esac
fi

# 远端版本必须认识这个升级名:在临时副本上试
info "远端版本是否认识升级 $NAME"
remote "set -e; cd $DEPLOY; T=\$(mktemp -d)
  cp config/genesis.toml \$T/genesis.toml
  printf '\n[[upgrades]]\nname = \"%s\"\nheight = %s\n' '$NAME' '$HEIGHT' >> \$T/genesis.toml
  sed \"s|^genesis = .*|genesis = \\\"\$T/genesis.toml\\\"|\" config/node21.toml > \$T/node.toml
  ./bin/node genesis-hash --config config/node21.toml | head -1 > \$T/before
  ./bin/node genesis-hash --config \$T/node.toml > \$T/after
  head -1 \$T/after | diff -q \$T/before - >/dev/null || { echo '创世哈希变了!中止' >&2; rm -rf \$T; exit 1; }
  echo \"  创世哈希不变:\$(cat \$T/before)\"
  echo \"  新时刻表:\$(tail -1 \$T/after)\"
  rm -rf \$T"

if [ "$DRY" = 1 ]; then
  info "dry-run:到此为止,没有改动任何东西"
  exit 0
fi

info "备份并追加 [[upgrades]]"
STAMP="$(date +%Y%m%d-%H%M%S)"
remote "set -e; cd $DEPLOY/config
  cp -p genesis.toml genesis.toml.bak-$STAMP
  printf '\n# 按高度激活的协议升级(deploy/activate-upgrade.sh,%s)\n[[upgrades]]\nname = \"%s\"\nheight = %s\n' '$STAMP' '$NAME' '$HEIGHT' >> genesis.toml
  echo \"  备份:config/genesis.toml.bak-$STAMP\""

info "4 个节点的时刻表必须一致"
LINES="$(remote "cd $DEPLOY && for n in 21 22 23 24; do ./bin/node genesis-hash --config config/node\$n.toml | tail -1; done")"
echo "$LINES" | sed 's/^/  /'
[ "$(echo "$LINES" | sort -u | wc -l | tr -d ' ')" = 1 ] || fail "节点之间时刻表不一致(不应发生:它们共用同一份创世)"
echo "$LINES" | head -1 | grep -q "$NAME@$HEIGHT" || fail "时刻表里没有 $NAME@$HEIGHT"

info "重启节点(重放全部历史,约几分钟)"
remote "cd $DEPLOY && scripts/starter.sh restart" | tail -6 | sed 's/^/  /'
remote "cd $DEPLOY && scripts/health-check.sh --level all --retries 60 --interval 6" | tail -3 | sed 's/^/  /' \
  || fail "健康检查没通过;回退:cp config/genesis.toml.bak-$STAMP config/genesis.toml && scripts/starter.sh restart"

info "核对"
H1="$(remote "curl -s $RPC_LOCAL/height" | tr -dc 0-9)"; sleep 10
H2="$(remote "curl -s $RPC_LOCAL/height" | tr -dc 0-9)"
echo "  高度 $H1 → $H2(在出块)"
[ "$H2" -gt "$H1" ] || fail "重启后不出块"
remote "curl -s $RPC_LOCAL/economy | head -1" | sed 's/^/  经济:/'
echo
echo "完成:$NAME 将在高度 $HEIGHT 激活(还有约 $(( (HEIGHT - H2) / 3600 )) 小时)。"
echo "回退(激活前):cd $DEPLOY && cp config/genesis.toml.bak-$STAMP config/genesis.toml && scripts/starter.sh restart"
