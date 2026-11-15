#!/usr/bin/env bash
# 知行图测试网:启停 4 个验证节点(社区规范 PACKAGING.md「Starter 接口」)。
#
#   scripts/starter.sh [start|stop|restart|status]
#
# - 无参数等同于 start;脚本以安装包目录为根,通过相对路径定位本包文件;
# - stop:先 SIGTERM,等待 STOP_TIMEOUT_SECONDS(默认 30);超时才 SIGKILL,再等
#   FORCE_STOP_TIMEOUT_SECONDS(默认 5)确认退出;**确认失败或无法确认时返回非零**;
# - 只清除已确认退出进程的 PID 文件;PID 被其他程序复用时不误杀;
# - restart 只有在 stop 成功后才 start;
# - start 不以"端口占用 / 残留 PID / 旧进程存在"作为成功依据,启动后逐个确认新进程存活。
#
# 防双签(同一验证人密钥同时运行两份 = 双签):
# 1. 识别"本节点进程"不依赖 PID 文件和路径字符串:扫描所有 `node run --config X`,读出 X 的 data_dir,
#    与本节点的 data_dir(解析为真实路径)比较。经软链接或其他版本目录启动的同一节点也能识别。
# 2. 每个节点进程由**节点程序自己**持有 <data_dir>/.node.lock 的独占锁;同一数据目录的第二份进程
#    无法启动(内核保证,即使 PID 文件丢失或脚本判断出错),手工启动二进制同样受保护。
# 3. 发现同一数据目录已有进程在运行时,start 拒绝启动;stop 停掉它(无论它从哪个目录启动)。
set -euo pipefail

ACTION="${1:-start}"
# 真实路径:避免"经软链接启动、经真实路径停止"时识别不到(2026-11-14 的事故原因)
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
CONFIG_DIR="${ZHIXING_CONFIG_DIR:-$ROOT_DIR/config}"
RUN_DIR="${RUN_DIR:-$ROOT_DIR/run}"
LOG_DIR="${LOG_DIR:-$ROOT_DIR/logs}"
BIN="$ROOT_DIR/bin/node"
NODES="${ZHIXING_NODES:-21 22 23 24}"
START_WAIT_SECONDS="${START_WAIT_SECONDS:-5}"
STOP_TIMEOUT_SECONDS="${STOP_TIMEOUT_SECONDS:-30}"
FORCE_STOP_TIMEOUT_SECONDS="${FORCE_STOP_TIMEOUT_SECONDS:-5}"
OPS_LOG="${OPS_LOG:-/opt/logs/zhixing-testnet-starter.log}"

ts() { date '+%Y-%m-%d %H:%M:%S'; }
log() {
  local line="[$(ts)] $*"
  printf '%s\n' "$line"
  if [[ -n "$OPS_LOG" ]] && mkdir -p "$(dirname "$OPS_LOG")" 2>/dev/null; then
    # 单个操作日志不超过 1 MB(目录规划规范 3.4)
    if [[ -f "$OPS_LOG" ]] && [[ "$(stat -c %s "$OPS_LOG" 2>/dev/null || echo 0)" -ge 1048576 ]]; then
      printf '[%s] truncated: log exceeded 1MB\n' "$(ts)" > "$OPS_LOG"
    fi
    printf '%s\n' "$line" >> "$OPS_LOG" 2>/dev/null || true
  fi
}
fail() { log "ERROR: $*" >&2; exit 1; }

pid_file() { printf '%s/n%s.pid' "$RUN_DIR" "$1"; }
node_cfg() { printf '%s/node%s.toml' "$CONFIG_DIR" "$1"; }

# 配置文件里的 data_dir,解析为真实路径
cfg_data_dir() {
  local cfg="$1" d
  d="$(sed -n 's/^data_dir[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' "$cfg" 2>/dev/null | head -1)"
  [[ -n "$d" ]] || return 1
  if [[ "$d" != /* ]]; then d="$(dirname "$cfg")/$d"; fi
  readlink -f "$d" 2>/dev/null || printf '%s' "$d"
}

# 进程的 --config 参数(解析为真实路径);不是 `node run --config X` 的进程返回失败
proc_config() {
  local pid="$1" args=() i
  [[ -r "/proc/$pid/cmdline" ]] || return 1
  mapfile -d '' -t args < "/proc/$pid/cmdline" 2>/dev/null || return 1
  [[ ${#args[@]} -ge 4 ]] || return 1
  [[ "$(basename "${args[0]}")" == node && "${args[1]}" == run ]] || return 1
  for ((i = 2; i < ${#args[@]} - 1; i++)); do
    if [[ "${args[$i]}" == --config ]]; then
      readlink -f "${args[$((i+1))]}" 2>/dev/null || printf '%s' "${args[$((i+1))]}"
      return 0
    fi
  done
  return 1
}

# 所有正在使用某数据目录的节点进程 PID(无论从哪个目录、经什么路径启动)
pids_using_data_dir() {
  local want="$1" p cfg dd
  for p in /proc/[0-9]*; do
    p="${p#/proc/}"
    cfg="$(proc_config "$p")" || continue
    dd="$(cfg_data_dir "$cfg")" || continue
    [[ "$dd" == "$want" ]] && printf '%s\n' "$p"
  done
  return 0
}

node_data_dir() {
  local cfg; cfg="$(node_cfg "$1")"
  [[ -f "$cfg" ]] || fail "missing config $cfg"
  cfg_data_dir "$cfg" || fail "no data_dir in $cfg"
}

load_password() {
  local pwf="$CONFIG_DIR/password"
  [[ -f "$pwf" ]] || fail "missing $pwf"
  ZHIXING_KEYSTORE_PASSWORD="$(cat "$pwf")"
  export ZHIXING_KEYSTORE_PASSWORD
}

start_node() {
  local id="$1" cfg dd running
  cfg="$(node_cfg "$id")"
  dd="$(node_data_dir "$id")"
  [[ -d "$dd" ]] || fail "n$id: data dir $dd does not exist"
  running="$(pids_using_data_dir "$dd" | tr '\n' ' ')"
  if [[ -n "${running// /}" ]]; then
    # 已有进程在用这个数据目录:如果正是本包启动的(PID 文件一致)就视为已运行;否则拒绝,避免双签
    local mine; mine="$(cat "$(pid_file "$id")" 2>/dev/null || true)"
    if [[ -n "$mine" && " $running " == *" $mine "* ]]; then
      log "n$id already running (pid $mine)"
      return 0
    fi
    fail "n$id: data dir $dd is already used by pid(s) ${running% } (another copy?) — refusing to start a second instance"
  fi
  rm -f "$(pid_file "$id")"
  # 数据目录锁由**节点程序自己**持有(见 node/src/store.rs 的 DataDirLock):
  # 节点启动时对 <data_dir>/.node.lock 取独占锁,取不到就直接失败。
  # 脚本不再另取一把锁——同一文件的两把 flock 会互相冲突,而且脚本的锁覆盖不到手工启动。
  (
    RUST_LOG="${RUST_LOG:-info}" exec "$BIN" run --config "$cfg"
  ) >> "$LOG_DIR/n$id.log" 2>&1 < /dev/null &
  echo $! > "$(pid_file "$id")"
  log "n$id started (pid $!)"
}

do_start() {
  [[ -x "$BIN" ]] || fail "missing binary $BIN"
  mkdir -p "$RUN_DIR" "$LOG_DIR"
  load_password
  local id
  for id in $NODES; do start_node "$id"; done
  unset ZHIXING_KEYSTORE_PASSWORD
  sleep "$START_WAIT_SECONDS"
  local bad=0 pid dd
  for id in $NODES; do
    pid="$(cat "$(pid_file "$id")" 2>/dev/null || true)"
    dd="$(node_data_dir "$id")"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && pids_using_data_dir "$dd" | grep -qx "$pid"; then
      log "n$id alive (pid $pid)"
    else
      log "ERROR: n$id is not running after start (see $LOG_DIR/n$id.log)"; bad=1
    fi
  done
  [[ "$bad" = 0 ]] || fail "start failed"
  log "start ok"
}

wait_exit() { # pid seconds
  local pid="$1" i
  for ((i = 0; i < $2; i++)); do kill -0 "$pid" 2>/dev/null || return 0; sleep 1; done
  ! kill -0 "$pid" 2>/dev/null
}

stop_node() {
  local id="$1" dd pids pid rc=0
  dd="$(node_data_dir "$id")"
  # 以"谁在用这个数据目录"为准,而不是 PID 文件:经软链接、其他版本目录启动的同一节点都会被找到
  pids="$(pids_using_data_dir "$dd")"
  if [[ -z "$pids" ]]; then
    rm -f "$(pid_file "$id")"
    log "n$id not running"
    return 0
  fi
  for pid in $pids; do
    log "n$id: SIGTERM pid $pid"
    kill -TERM "$pid" 2>/dev/null || true
  done
  for pid in $pids; do
    if ! wait_exit "$pid" "$STOP_TIMEOUT_SECONDS"; then
      log "n$id: pid $pid still running after ${STOP_TIMEOUT_SECONDS}s, SIGKILL"
      kill -KILL "$pid" 2>/dev/null || true
      wait_exit "$pid" "$FORCE_STOP_TIMEOUT_SECONDS" || { log "ERROR: n$id pid $pid did not exit"; rc=1; }
    fi
  done
  # 再次确认:这个数据目录上不应再有任何节点进程
  if [[ -n "$(pids_using_data_dir "$dd")" ]]; then
    log "ERROR: n$id: data dir $dd is still in use after stop"
    return 1
  fi
  [[ "$rc" = 0 ]] || return 1
  rm -f "$(pid_file "$id")"
  log "n$id stopped"
}

do_stop() {
  local id rc=0
  for id in $NODES; do stop_node "$id" || rc=1; done
  [[ "$rc" = 0 ]] || fail "stop failed: some node did not exit"
  log "stop ok"
}

do_status() {
  local id dd pids rc=0
  for id in $NODES; do
    dd="$(node_data_dir "$id")"
    pids="$(pids_using_data_dir "$dd" | tr '\n' ' ')"
    if [[ -n "${pids// /}" ]]; then echo "n$id running (pid ${pids% })"; else echo "n$id stopped"; rc=1; fi
  done
  return "$rc"
}

case "$ACTION" in
  start) do_start ;;
  stop) do_stop ;;
  restart) do_stop && do_start ;;
  status) do_status ;;
  -h|--help) echo "Usage: $0 [start|stop|restart|status]"; exit 0 ;;
  *) echo "Usage: $0 [start|stop|restart|status]" >&2; exit 2 ;;
esac
