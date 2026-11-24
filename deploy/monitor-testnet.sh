#!/usr/bin/env bash
# 知行图测试网监控:链活性、验证人、通道、磁盘。发现异常时通过飞书通知。
#
# 遵循社区运维规范(OPERATIONAL_GUIDANCE.MD §3.5):定时任务只调用脚本,脚本把输出写进日志文件,
# 日志按大小截断;脚本失败要能被调度器看到(非零退出)。
#
# 检查项:
#   1. 4 个验证节点进程在、RPC 可答;
#   2. 链在出块(高度在增长);
#   3. 4 个节点高度与 head 一致;
#   4. 旧机 → 新机的 SSH 通道通(旧机上跑;浏览器依赖它);
#   5. 磁盘余量;
#   6. 正文镜像(本机有副本目录时):副本是否齐全、按时更新,主正文仓库是否丢了正文。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
LOGFILE="${LOGFILE:-/opt/logs/zhixing-testnet-monitor.log}"
RPC="${ZHIXING_RPC:-http://127.0.0.1:9221}"
NODES="${ZHIXING_NODES:-21 22 23 24}"
TUNNEL_URL="${ZHIXING_TUNNEL_URL:-http://127.0.0.1:19221}"
MIN_FREE_MB="${ZHIXING_MIN_FREE_MB:-2000}"
STALL_SECONDS="${ZHIXING_STALL_SECONDS:-300}"
STATE_FILE="${STATE_FILE:-/var/lib/zhixing-monitor.state}"
BODY_MIRROR_DIR="${BODY_MIRROR_DIR:-/data/zhixing-testnet/bodies-mirror}"
BODY_MIRROR_MAX_AGE="${BODY_MIRROR_MAX_AGE:-10800}"   # 每小时镜像一次;3 小时没更新就告警
FEISHU_SCENE="monitor_service"

# 飞书通知(社区公共库);缺失时只记日志不报错
if [[ -f /root/code/deployer/operator/feishu-notify/common.sh ]]; then
  # shellcheck disable=SC1091
  source /root/code/deployer/operator/feishu-notify/common.sh
fi

log() {
  local line="[$(date '+%Y-%m-%d %H:%M:%S')] $*"
  printf '%s\n' "$line"
  mkdir -p "$(dirname "$LOGFILE")" 2>/dev/null || true
  if [[ -f "$LOGFILE" ]] && [[ "$(stat -c %s "$LOGFILE" 2>/dev/null || echo 0)" -ge 1048576 ]]; then
    printf '[%s] truncated: log exceeded 1MB\n' "$(date)" > "$LOGFILE"
  fi
  printf '%s\n' "$line" >> "$LOGFILE" 2>/dev/null || true
}

notify() {
  local message="$1"
  if declare -F send_feishu_message >/dev/null 2>&1; then
    if ! send_feishu_message "$FEISHU_SCENE" "$message" >> "$LOGFILE" 2>&1; then
      log "WARN! failed to send feishu notification"
    fi
  else
    log "WARN! feishu helper missing, notification skipped"
  fi
}

problems=()
check() { # name ok? detail
  if [[ "$2" = "0" ]]; then
    log "PASS $1: $3"
  else
    log "FAIL $1: $3"
    problems+=("$1: $3")
  fi
}

rpc_get() { curl -fsS --max-time 5 "$RPC$1" 2>/dev/null; }

# 1. 节点进程与 RPC(只在节点所在的机器上检查;旧机只有浏览器与通道)
if [ "${ZHIXING_SKIP_NODES:-0}" = "1" ]; then
  log "SKIP node checks (ZHIXING_SKIP_NODES=1:本机不运行验证节点)"
else
for id in $NODES; do
  if curl -fsS --max-time 5 "http://127.0.0.1:92$id/height" >/dev/null 2>&1; then
    check "node-n$id" 0 "RPC 可答"
  else
    check "node-n$id" 1 "RPC 无响应"
  fi
done
fi

# 2/3. 高度增长与节点一致(同上:不跑节点就不查)
if [ "${ZHIXING_SKIP_NODES:-0}" = "1" ]; then
  log "SKIP chain progress and agreement checks"
else
h0=""
for id in $NODES; do
  v="$(curl -fsS --max-time 5 "http://127.0.0.1:92$id/height" 2>/dev/null || true)"
  [ -z "$h0" ] && h0="$v"
  if [ -z "$v" ]; then check "height-n$id" 1 "读不到高度"; fi
done
sleep 3
h1="$(curl -fsS --max-time 5 "http://127.0.0.1:92${NODES%% *}/height" 2>/dev/null || true)"
prev="$(cat "$STATE_FILE" 2>/dev/null || true)"   # 格式: 时间 高度
prev_t="$(echo "$prev" | awk '{print $1+0}')"
prev_h="$(echo "$prev" | awk '{print $2+0}')"
now_t="$(date +%s)"
if [ -n "$h1" ] && [ "$h1" -gt 0 ] 2>/dev/null; then
  if [ "$prev_h" -gt 0 ] && [ "$now_t" -gt "$prev_t" ]; then
    rate=$(( (h1 - prev_h) * 100 / (now_t - prev_t) ))
    if [ "$rate" -le 0 ] && [ $(( now_t - prev_t )) -ge "$STALL_SECONDS" ]; then
      check "chain-progress" 1 "链已停滞 $(( now_t - prev_t ))s(高度停在 $prev_h)"
    else
      check "chain-progress" 0 "高度 $prev_h → $h1(${rate}/min)"
    fi
  else
    check "chain-progress" 0 "高度 $h1(首次采样)"
  fi
  echo "$now_t $h1" > "$STATE_FILE"
else
  check "chain-progress" 1 "读不到高度"
fi

# 一致性:在同一个高度上比较块哈希。
# 不能直接比各节点的最新 head:链约 0.7 秒出一块,逐个读取的间隙里出了新块,
# 先读的节点还是旧块、后读的已是新块,会误报"不一致"(2026-11-15~19 共 58 次误报)。
# 做法与 scripts/health-check.sh 相同:取所有节点都已有的高度(最低高度 - 1),比较该块的哈希。
min_h=""
for id in $NODES; do
  v="$(curl -fsS --max-time 5 "http://127.0.0.1:92$id/height" 2>/dev/null || true)"
  if ! [ "$v" -ge 0 ] 2>/dev/null; then min_h=""; break; fi
  if [ -z "$min_h" ] || [ "$v" -lt "$min_h" ]; then min_h="$v"; fi
done
if [ -z "$min_h" ] || [ "$min_h" -lt 1 ]; then
  check "node-agreement" 1 "读不到全部节点的高度,无法比较"
else
  at=$((min_h - 1))
  hashes=""
  for id in $NODES; do
    hh="$(curl -fsS --max-time 5 "http://127.0.0.1:92$id/block/$at" 2>/dev/null | sed -n 's/^hash=//p' || true)"
    hashes="$hashes ${hh:-?}"
  done
  uniq_hashes="$(echo $hashes | tr ' ' '\n' | grep -v '^$' | sort -u | wc -l | tr -d ' ')"
  if [ "$uniq_hashes" = "1" ] && ! echo "$hashes" | grep -qF '?'; then
    check "node-agreement" 0 "4 个节点在第 $at 块一致($(echo $hashes | awk '{print substr($1,1,16)}')…)"
  else
    check "node-agreement" 1 "节点在第 $at 块不一致:$hashes"
  fi
fi
fi

# 4. 通道(在旧机上跑才有意义;本机跑时跳过)
if [ -n "${ZHIXING_CHECK_TUNNEL:-}" ]; then
  if curl -fsS --max-time 5 "$TUNNEL_URL/height" >/dev/null 2>&1; then
    check "tunnel" 0 "旧机 → 新机 通道可答"
  else
    check "tunnel" 1 "通道不可达(浏览器读不到链)"
  fi
fi

# 5. 磁盘
free_mb="$(df -Pm /data 2>/dev/null | awk 'NR==2 {print $4}')"
if [ -z "$free_mb" ]; then
  check "disk" 0 "/data 用量不可读(跳过)"
elif [ "$free_mb" -lt "$MIN_FREE_MB" ]; then
  check "disk" 1 "/data 剩余 ${free_mb} MB(低于 ${MIN_FREE_MB} MB)"
else
  check "disk" 0 "/data 剩余 ${free_mb} MB"
fi

# 6. 正文镜像:链上只存哈希,正文丢了哈希就成空壳。读 body-mirror.sh 写的 STATUS.json
if [ -f "$BODY_MIRROR_DIR/STATUS.json" ]; then
  verdict="$(python3 - "$BODY_MIRROR_DIR/STATUS.json" "$BODY_MIRROR_MAX_AGE" <<'PYEOF' 2>&1
import json, sys, time
s = json.load(open(sys.argv[1])); max_age = int(sys.argv[2])
age = int(time.time()) - int(s.get("unix", 0))
bad = []
if age > max_age:
    bad.append(f"副本 {age // 3600} 小时没有更新")
if s.get("missing"):
    bad.append(f"副本缺 {len(s['missing'])} 份(哪里都取不到),例:{s['missing'][0]['refs'][0]}")
if s.get("source_missing"):
    bad.append(f"主正文仓库缺 {len(s['source_missing'])} 份,例:{s['source_missing'][0]['refs'][0]}")
print(("1 " + ";".join(bad)) if bad else f"0 副本 {s.get('have')}/{s.get('refs')} 份齐全,主仓库 {s.get('source_ok', '?')}/{s.get('refs')}")
PYEOF
)"
  case "$verdict" in
    0\ *) check "bodies" 0 "${verdict#0 }" ;;
    1\ *) check "bodies" 1 "${verdict#1 }" ;;
    *)    check "bodies" 1 "正文镜像状态读不了:$verdict" ;;
  esac
fi

if [ "${#problems[@]}" -gt 0 ]; then
  {
    echo "【异常通知】知行图测试网异常"
    echo
    echo "发现时间：$(date '+%Y-%m-%d %H:%M')"
    echo "异常等级：P1"
    echo "当前状态：已发现，待处理"
    echo
    echo "=== 异常项 ==="
    for p in "${problems[@]}"; do echo "- $p"; done
    echo
    echo "=== 处理建议 ==="
    echo "1. 检查节点:systemctl status zhixing-testnet;journalctl -u zhixing-testnet -n 50"
    echo "2. 检查健康:cd /opt/deploy/zhixing-testnet && scripts/health-check.sh --level all"
    echo "3. 旧机通道:systemctl status zhixing-tunnel"
    echo "4. 正文:tail /opt/logs/zhixing-body-mirror.log;主仓库缺的用副本恢复,见 deploy/README.md「正文镜像」"
  } > /tmp/zhixing-alert.$$
  notify "$(cat /tmp/zhixing-alert.$$)"
  rm -f /tmp/zhixing-alert.$$
  log "ALERT: ${#problems[@]} 项异常已通知"
  exit 1
fi

log "OK: 全部检查通过"
exit 0
