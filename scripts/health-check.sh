#!/usr/bin/env bash
# 知行图测试网:健康检查(社区规范 HEALTH_CHECK.md)。
#
#   scripts/health-check.sh [--level liveness|readiness|dependency|all] [--timeout S] [--retries N]
#                           [--interval S] [--format text|json] [--quiet] [--wait S] [--verbose]
#
# 退出码:0 全部必选检查通过;1 有检查失败;2 用法/参数错误;3 检查框架自身异常;4 整体超时。
# 只读、幂等:不重启服务、不改配置、不清数据。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

LEVEL="${HEALTH_LEVEL:-readiness}"
TIMEOUT="${HEALTH_TIMEOUT:-10}"
RETRIES="${HEALTH_RETRIES:-0}"
INTERVAL="${HEALTH_INTERVAL:-2}"
FORMAT="${HEALTH_FORMAT:-text}"
QUIET=false
VERBOSE=false
WAIT=0

PROJECT_NAME="zhixing-testnet"
PROJECT_VERSION="unknown"
ENVIRONMENT="${ZHIXING_ENV:-testnet}"
CONFIG_DIR="${ZHIXING_CONFIG_DIR:-$PROJECT_DIR/config}"
RUN_DIR="${RUN_DIR:-$PROJECT_DIR/run}"
BIN="$PROJECT_DIR/bin/node"
NODES="${ZHIXING_NODES:-21 22 23 24}"
LOGFILE="${ZHIXING_OPS_LOG:-/opt/logs/zhixing-testnet-health.log}"

PASSED=0; WARNED=0; FAILED=0; SKIPPED=0
CHECK_NAMES=(); CHECK_STATUSES=(); CHECK_MS=(); CHECK_MSGS=()

usage() {
  cat >&2 <<'EOF'
Usage: scripts/health-check.sh [--level liveness|readiness|dependency|all] [--timeout seconds]
       [--retries count] [--interval seconds] [--format text|json] [--quiet] [--wait seconds] [--verbose]
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --level) LEVEL="${2:-}"; shift 2 ;;
    --timeout) TIMEOUT="${2:-}"; shift 2 ;;
    --retries) RETRIES="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --format) FORMAT="${2:-}"; shift 2 ;;
    --wait) WAIT="${2:-}"; shift 2 ;;
    --quiet) QUIET=true; shift ;;
    --verbose) VERBOSE=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$LEVEL" in liveness|readiness|dependency|all) ;; *) echo "ERROR: invalid --level: $LEVEL" >&2; exit 2 ;; esac
case "$FORMAT" in text|json) ;; *) echo "ERROR: invalid --format: $FORMAT" >&2; exit 2 ;; esac
[[ "$TIMEOUT" =~ ^[0-9]+$ ]] || { echo "ERROR: --timeout must be a number" >&2; exit 2; }
[[ "$RETRIES" =~ ^[0-9]+$ ]] || { echo "ERROR: --retries must be a number" >&2; exit 2; }
[[ "$INTERVAL" =~ ^[0-9]+$ ]] || { echo "ERROR: --interval must be a number" >&2; exit 2; }
[[ "$WAIT" =~ ^[0-9]+$ ]] || { echo "ERROR: --wait must be a number" >&2; exit 2; }
command -v curl >/dev/null 2>&1 || { echo "ERROR: curl is required" >&2; exit 3; }

START_MS=$(date +%s%3N)
STARTED_AT="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

init_log() {
  mkdir -p "$(dirname "$LOGFILE")" 2>/dev/null || true
  touch "$LOGFILE" 2>/dev/null || true
  if [[ -f "$LOGFILE" ]] && [[ "$(stat -c %s "$LOGFILE" 2>/dev/null || echo 0)" -ge 1048576 ]]; then
    printf '[%s] truncated: log exceeded 1MB\n' "$(date)" > "$LOGFILE"
  fi
}
init_log
log() { [[ "$QUIET" = true ]] || printf '%s\n' "$*" >&2; printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >> "$LOGFILE" 2>/dev/null || true; }

record() { # name status ms message
  CHECK_NAMES+=("$1"); CHECK_STATUSES+=("$2"); CHECK_MS+=("$3"); CHECK_MSGS+=("$4")
  case "$2" in
    PASS) PASSED=$((PASSED+1)) ;;
    WARN) WARNED=$((WARNED+1)) ;;
    FAIL) FAILED=$((FAILED+1)) ;;
    SKIP) SKIPPED=$((SKIPPED+1)) ;;
  esac
  [[ "$QUIET" = true ]] || printf '[%s] %s: %s (%s ms)\n' "$2" "$1" "$4" "$3"
}

now_ms() { date +%s%3N; }

# 重试:每次都重新检查
retry() { # fn
  local attempt=0
  while true; do
    if "$@"; then return 0; fi
    if [[ "$attempt" -ge "$RETRIES" ]]; then return 1; fi
    attempt=$((attempt+1)); sleep "$INTERVAL"
  done
}

rpc_get() { # port path
  curl -fsS --max-time "$TIMEOUT" "http://127.0.0.1:$1$2" 2>/dev/null
}

check_liveness() {
  local id pid t0 t1 alive=0
  for id in $NODES; do
    t0=$(now_ms)
    pid="$(cat "$RUN_DIR/n$id.pid" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      t1=$(now_ms); record "process:n$id" PASS $((t1-t0)) "node process is running (pid $pid)"; alive=$((alive+1))
    else
      t1=$(now_ms); record "process:n$id" FAIL $((t1-t0)) "node process is not running"
    fi
  done
  [[ "$alive" -gt 0 ]]
}

check_rpc_ports() {
  local p t0 t1 up=0
  for p in $(for id in $NODES; do echo "92$id"; done); do
    t0=$(now_ms)
    if rpc_get "$p" /height >/dev/null; then
      t1=$(now_ms); record "rpc:$p" PASS $((t1-t0)) "RPC endpoint answered /height"; up=$((up+1))
    else
      t1=$(now_ms); record "rpc:$p" FAIL $((t1-t0)) "RPC endpoint did not answer /height"
    fi
  done
  [[ "$up" -gt 0 ]]
}

check_readiness() {
  local h0 h1 t0 t1 same=1 heights=()
  t0=$(now_ms)
  local id
  for id in $NODES; do
    h0="$(rpc_get "92$id" /height || true)"
    [[ -n "$h0" ]] || h0="?"
    heights+=("$h0")
  done
  # 高度必须推进(链在出块)
  sleep 3
  for id in $NODES; do
    h1="$(rpc_get "92$id" /height || true)"
    if [[ -n "$h1" && "$h1" != "?" && "${h0:-?}" != "?" ]]; then
      if [[ "$h1" -le "${h0:-0}" ]]; then same=0; fi
    else
      same=0
    fi
  done
  t1=$(now_ms)
  if [[ "$same" = 1 ]]; then
    record "chain:progress" PASS $((t1-t0)) "heights advancing: ${heights[*]}"
  else
    record "chain:progress" FAIL $((t1-t0)) "heights not advancing on all nodes: ${heights[*]}"
  fi
  # 4 个节点的 head 必须一致
  local heads=() h
  for id in $NODES; do h="$(rpc_get "92$id" /head || true)"; heads+=("${h:-?}"); done
  local uniq
  uniq="$(printf '%s\n' "${heads[@]}" | sort -u | wc -l)"
  t0=$(now_ms); t1=$(now_ms)
  if [[ "$uniq" = 1 && "${heads[0]}" != "?" ]]; then
    record "chain:agreement" PASS $((t1-t0)) "all nodes share head ${heads[0]:0:16}…"
  else
    record "chain:agreement" FAIL $((t1-t0)) "nodes disagree: ${heads[*]}"
  fi
  [[ "$same" = 1 && "$uniq" = 1 ]]
}

check_dependency() {
  local t0 t1 pct
  t0=$(now_ms)
  pct="$(df -P /data 2>/dev/null | awk 'NR==2 {gsub("%","",$5); print $5}')"
  if [[ -z "$pct" ]]; then
    t1=$(now_ms); record "disk:/data" SKIP $((t1-t0)) "cannot read /data usage"
  elif [[ "$pct" -ge 90 ]]; then
    t1=$(now_ms); record "disk:/data" FAIL $((t1-t0)) "/data is ${pct}% full"
  elif [[ "$pct" -ge 80 ]]; then
    t1=$(now_ms); record "disk:/data" WARN $((t1-t0)) "/data is ${pct}% full"
  else
    t1=$(now_ms); record "disk:/data" PASS $((t1-t0)) "/data is ${pct}% used"
  fi
  [[ -z "$pct" || "$pct" -lt 90 ]]
}

run_level() {
  case "$1" in
    liveness) check_liveness && check_rpc_ports ;;
    readiness) check_rpc_ports && check_readiness ;;
    dependency) check_dependency ;;
  esac
}

# --wait:在总时限内等待服务就绪
if [[ "$WAIT" -gt 0 ]]; then
  deadline=$(( $(now_ms) + WAIT * 1000 ))
  while [[ "$(now_ms)" -lt "$deadline" ]]; do
    if rpc_get "92${NODES%% *}" /height >/dev/null 2>&1; then break; fi
    sleep 2
  done
fi

STATUS=pass
case "$LEVEL" in
  liveness) run_level liveness || STATUS=fail ;;
  readiness) run_level readiness || STATUS=fail ;;
  dependency) run_level dependency || STATUS=fail ;;
  all)
    run_level liveness || STATUS=fail
    run_level readiness || STATUS=fail
    run_level dependency || STATUS=fail
    ;;
esac

DURATION_MS=$(( $(now_ms) - START_MS ))
if [[ "$FAILED" -gt 0 ]]; then STATUS=fail; fi
SUMMARY="RESULT status=$STATUS passed=$PASSED warned=$WARNED failed=$FAILED skipped=$SKIPPED duration_ms=$DURATION_MS"
[[ "$QUIET" = true ]] || printf '%s\n' "$SUMMARY"

if [[ "$FORMAT" = json ]]; then
  esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\t/\\t/g' | tr -d '\000-\010\013\014\016-\037'; }
  printf '{'
  printf '"schema_version":"1.0","type":"health_check","project":"%s","version":"%s","environment":"%s",' "$PROJECT_NAME" "$PROJECT_VERSION" "$ENVIRONMENT"
  printf '"level":"%s","status":"%s","started_at":"%s","duration_ms":%s,' "$LEVEL" "$STATUS" "$STARTED_AT" "$DURATION_MS"
  printf '"summary":{"passed":%s,"warned":%s,"failed":%s,"skipped":%s},' "$PASSED" "$WARNED" "$FAILED" "$SKIPPED"
  printf '"checks":['
  for i in "${!CHECK_NAMES[@]}"; do
    [[ $i -gt 0 ]] && printf ','
    printf '{"name":"%s","status":"%s","duration_ms":%s,"message":"%s"}' \
      "$(esc "${CHECK_NAMES[$i]}")" "$(esc "${CHECK_STATUSES[$i]}")" "${CHECK_MS[$i]}" "$(esc "${CHECK_MSGS[$i]}")"
  done
  printf ']}\n'
fi

case "$STATUS" in
  pass) exit 0 ;;
  fail) exit 1 ;;
esac
