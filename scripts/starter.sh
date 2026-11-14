#!/usr/bin/env bash
# 知行图测试网:启停 4 个验证节点(社区规范 PACKAGING.md「Starter 接口」)。
#
#   scripts/starter.sh [start|stop|restart|status]
#
# - 无参数等同于 start;脚本以安装包目录为根,通过相对路径定位本包文件;
# - stop:先 SIGTERM,等待 STOP_TIMEOUT_SECONDS(默认 30);超时才 SIGKILL,再等
#   FORCE_STOP_TIMEOUT_SECONDS(默认 5)确认退出;确认失败返回非零;
# - 只清除已确认退出进程的 PID 文件;PID 文件对应的进程若不是本节点(PID 被复用),不误杀;
# - restart 只有在 stop 成功后才 start;
# - start 不以"端口占用 / 残留 PID / 旧进程存在"作为成功依据:启动后逐个确认新进程存活。
#
# 验证节点绝不能在两处同时运行(同一密钥在两台机器出块即双签);迁移与回退都必须先停一处、确认后再启另一处。
set -euo pipefail

ACTION="${1:-start}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
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

# PID 文件里的进程确实是本节点(命令行包含本节点的配置文件)才算数,防止 PID 复用误判/误杀
our_pid() {
  local id="$1" f pid
  f="$(pid_file "$id")"
  [[ -f "$f" ]] || return 1
  pid="$(cat "$f" 2>/dev/null || true)"
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -qF -- "$(node_cfg "$id")" || return 1
  printf '%s' "$pid"
}

load_password() {
  local pwf="$CONFIG_DIR/password"
  [[ -f "$pwf" ]] || fail "missing $pwf"
  ZHIXING_KEYSTORE_PASSWORD="$(cat "$pwf")"
  export ZHIXING_KEYSTORE_PASSWORD
}

start_node() {
  local id="$1" pid cfg
  cfg="$(node_cfg "$id")"
  [[ -f "$cfg" ]] || fail "missing config $cfg"
  if pid="$(our_pid "$id")"; then
    log "n$id already running (pid $pid)"
    return 0
  fi
  rm -f "$(pid_file "$id")"
  RUST_LOG="${RUST_LOG:-info}" nohup "$BIN" run --config "$cfg" >> "$LOG_DIR/n$id.log" 2>&1 < /dev/null &
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
  local bad=0
  for id in $NODES; do
    if our_pid "$id" >/dev/null; then log "n$id alive"; else log "ERROR: n$id exited right after start (see $LOG_DIR/n$id.log)"; bad=1; fi
  done
  [[ "$bad" = 0 ]] || fail "start failed"
  log "start ok"
}

stop_node() {
  local id="$1" pid i
  if ! pid="$(our_pid "$id")"; then
    # PID 文件失效:只清理文件,不碰同 PID 的其他进程
    rm -f "$(pid_file "$id")"
    log "n$id not running"
    return 0
  fi
  log "n$id: SIGTERM pid $pid"
  kill -TERM "$pid" 2>/dev/null || true
  for ((i = 0; i < STOP_TIMEOUT_SECONDS; i++)); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "$pid" 2>/dev/null; then
    log "n$id: still running after ${STOP_TIMEOUT_SECONDS}s, SIGKILL"
    kill -KILL "$pid" 2>/dev/null || true
    for ((i = 0; i < FORCE_STOP_TIMEOUT_SECONDS; i++)); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
  fi
  if kill -0 "$pid" 2>/dev/null; then
    log "ERROR: n$id pid $pid did not exit"
    return 1
  fi
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
  local id pid rc=0
  for id in $NODES; do
    if pid="$(our_pid "$id")"; then echo "n$id running (pid $pid)"; else echo "n$id stopped"; rc=1; fi
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
