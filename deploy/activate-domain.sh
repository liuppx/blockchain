#!/usr/bin/env bash
# 在测试网上排期一次"新增领域"(按高度激活)。
#
#   deploy/activate-domain.sh --id 3 --height H             # 排期:领域 3 从高度 H 起可用
#   deploy/activate-domain.sh --id 3 --height H --dry-run   # 只检查、只打印
#   deploy/activate-domain.sh --check                       # 看远端当前的领域时刻表
#
# **加领域不需要改代码、不需要发新版本**——领域集合写在创世的 `[[domains]] id = N height = H` 里,
# 和 `[[upgrades]]` 一样**不进创世哈希**:它是"这条链从哪个高度起多一个领域"的约定。
# 从 H* 起,"这一刻能用哪些领域"里多一个编号;`[knowledge] domains` 本身不改,
# 所以 H* 之前每个字节都与加领域前相同(已有区块照旧有效)。
#
# 领域名称(教育 / 运动健身 / …)只在浏览器的领域词表里定义
# (`tools/explorer/public/ns/v1/domains.jsonld`),不上链。网页只让成员选**链已接受**的领域,
# 所以词表里先加上、链还没激活时网页不会让人选它。
#
# 回退(H 之前随时可以):把备份的 genesis.toml 换回去,再 starter.sh restart。
#
# 不动:链数据、密钥、节点版本。
set -euo pipefail

HOST="${HOST:-root@221.130.30.75}"
SSH=(ssh -p 10022 "$HOST")
DEPLOY=/opt/deploy/zhixing-testnet
RPC_LOCAL=http://127.0.0.1:9221
DOMAIN_ID=""
HEIGHT=""
MIN_LEAD="${MIN_LEAD:-3600}"   # H 至少比当前高度高这么多块(约 1 小时),来得及安全重启
DRY=0
CHECK=0
while [ $# -gt 0 ]; do
  case "$1" in
    --id) DOMAIN_ID="$2"; shift 2 ;;
    --height) HEIGHT="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --check) CHECK=1; shift ;;
    *) echo "未知参数 $1" >&2; exit 2 ;;
  esac
done

info() { printf '\n== %s\n' "$*"; }
fail() { echo "ERROR: $*" >&2; exit 1; }
remote() { "${SSH[@]}" "$@"; }

if [ "$CHECK" = 1 ]; then
  info "远端领域时刻表"
  remote "cd $DEPLOY && for n in 21 22 23 24; do printf 'n%s: ' \$n; ./bin/node genesis-hash --config config/node\$n.toml | tail -1; done"
  echo
  remote "for p in 9221 9222 9223 9224; do printf ':%s ' \$p; curl -s --max-time 5 -H 'Accept: application/json' http://127.0.0.1:\$p/domains; echo; done"
  exit 0
fi

[ -n "$DOMAIN_ID" ] || fail "需要 --id N(领域编号)"
[[ "$DOMAIN_ID" =~ ^[0-9]+$ ]] || fail "--id 应为整数"
[ -n "$HEIGHT" ] || fail "需要 --height H"
[[ "$HEIGHT" =~ ^[0-9]+$ ]] || fail "--height 应为整数"

info "前置检查"
CUR="$(remote "curl -s --max-time 5 $RPC_LOCAL/height" | tr -dc 0-9)"
[ -n "$CUR" ] || fail "读不到远端高度"
echo "  当前高度 $CUR,激活高度 $HEIGHT(还有 $((HEIGHT - CUR)) 块,约 $(( (HEIGHT - CUR) / 3600 )) 小时)"
[ "$HEIGHT" -gt $((CUR + MIN_LEAD)) ] || fail "激活高度离现在太近(至少要领先 $MIN_LEAD 块),来不及安全重启"
remote "cd $DEPLOY && scripts/starter.sh status" | sed 's/^/  /'

# 同一个编号不能排两次:否则"哪个生效"说不清。
if remote "grep -qE '^id = $DOMAIN_ID$' $DEPLOY/config/genesis.toml"; then
  fail "远端创世里已经有 id = $DOMAIN_ID 的领域了;先用 --check 看清楚,不要重复追加"
fi

# 远端版本必须认识 `[[domains]]` 这一段:老版本解析创世时会忽略未知字段,
# 于是领域排了期也不生效(网页一直不让选)。所以要求远端节点提供 /domains。
info "远端版本是否实现了按高度新增领域(要先用 deploy-nodes.sh 部署新版本)"
VER_DIR="$(remote "readlink -f $DEPLOY")"
echo "  远端版本:$VER_DIR"
for p in 9221 9222 9223 9224; do
  DOM="$(remote "curl -s --max-time 5 -H 'Accept: application/json' http://127.0.0.1:$p/domains")"
  case "$DOM" in
    *'"next"'*) echo "  ✓ :$p /domains 可用" ;;
    *) fail "节点 :$p 没有实现 /domains(老版本):先部署新版本。响应:${DOM:0:120}" ;;
  esac
done
if [ -n "${EXPECT_COMMIT:-}" ]; then
  case "$VER_DIR" in *"$EXPECT_COMMIT"*) echo "  ✓ 版本是预期的提交 $EXPECT_COMMIT" ;;
    *) fail "远端版本不是预期的提交 $EXPECT_COMMIT" ;; esac
fi

info "远端版本是否认识 [[domains]]"
remote "set -e; cd $DEPLOY; T=\$(mktemp -d)
  cp config/genesis.toml \$T/genesis.toml
  printf '\n[[domains]]\nid = %s\nheight = %s\n' '$DOMAIN_ID' '$HEIGHT' >> \$T/genesis.toml
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

info "备份并追加 [[domains]]"
STAMP="$(date +%Y%m%d-%H%M%S)"
remote "set -e; cd $DEPLOY/config
  cp -p genesis.toml genesis.toml.bak-$STAMP
  printf '\n# 按高度新增的领域(deploy/activate-domain.sh,%s)\n[[domains]]\nid = %s\nheight = %s\n' '$STAMP' '$DOMAIN_ID' '$HEIGHT' >> genesis.toml
  echo \"  备份:config/genesis.toml.bak-$STAMP\""

on_fail() {
  echo >&2
  echo "!! 排期中途失败。创世已追加 [[domains]],备份在 config/genesis.toml.bak-$STAMP。" >&2
  echo "!! 激活高度 $HEIGHT 之前可以安全回退:" >&2
  echo "!!   ssh -p 10022 $HOST 'cd $DEPLOY && cp config/genesis.toml.bak-$STAMP config/genesis.toml && scripts/starter.sh restart'" >&2
}
trap on_fail ERR

info "4 个节点的时刻表必须一致"
LINES="$(remote "cd $DEPLOY && for n in 21 22 23 24; do ./bin/node genesis-hash --config config/node\$n.toml | tail -1; done")"
echo "$LINES" | sed 's/^/  /'
[ "$(echo "$LINES" | sort -u | wc -l | tr -d ' ')" = 1 ] || fail "节点之间时刻表不一致(不应发生:它们共用同一份创世)"
echo "$LINES" | head -1 | grep -q "domain-$DOMAIN_ID@$HEIGHT" || fail "时刻表里没有 domain-$DOMAIN_ID@$HEIGHT"

info "重启节点(重放全部历史,约几分钟)"
remote "cd $DEPLOY && scripts/starter.sh restart" | tail -6 | sed 's/^/  /'
remote "cd $DEPLOY && scripts/health-check.sh --level all --retries 60 --interval 6" | tail -3 | sed 's/^/  /' \
  || fail "健康检查没通过;回退:cp config/genesis.toml.bak-$STAMP config/genesis.toml && scripts/starter.sh restart"

info "核对:每个节点都要报同一个领域集合"
for p in 9221 9222 9223 9224; do
  DOM="$(remote "curl -s --max-time 5 -H 'Accept: application/json' http://127.0.0.1:$p/domains")"
  echo "  :$p $DOM"
  case "$DOM" in
    *"\"$DOMAIN_ID\""*) ;;
    *) fail "节点 :$p 的领域集合里没有 $DOMAIN_ID(它没换上新时刻表?):$DOM" ;;
  esac
done
H1="$(remote "curl -s $RPC_LOCAL/height" | tr -dc 0-9)"; sleep 10
H2="$(remote "curl -s $RPC_LOCAL/height" | tr -dc 0-9)"
echo "  高度 $H1 → $H2(在出块)"
[ "$H2" -gt "$H1" ] || fail "重启后不出块"
trap - ERR
echo
echo "完成:领域 $DOMAIN_ID 将在高度 $HEIGHT 激活(还有约 $(( (HEIGHT - H2) / 3600 )) 小时)。"
echo "回退(激活前):cd $DEPLOY && cp config/genesis.toml.bak-$STAMP config/genesis.toml && scripts/starter.sh restart"
