#!/usr/bin/env bash
# 知行图测试网:自动化测试统一入口(社区规范 AUTOMATED_TESTING.md)。
#
#   scripts/test.sh [--suite unit|smoke|all] [--base-url URL] [--timeout S] [--format text|json]
#                   [--output PATH] [--case ID]... [--fail-fast] [--quiet]
#
# - unit:节点的 Rust 单元测试(需要源码 checkout 与 cargo;安装包内没有源码时返回 3);
# - smoke:部署后只读冒烟,**不写链**,可对生产地址执行。默认对本机 n21 的 RPC(127.0.0.1:9221),
#   也可用 --base-url 指向 https://blockchain.yeying.pub/rpc;
# - 退出码:0 通过;1 有用例失败;2 参数或用例选择错误;3 环境异常;4 整体超时。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

SUITE="${TEST_SUITE:-unit}"
BASE_URL="${TEST_BASE_URL:-http://127.0.0.1:9221}"
TIMEOUT="${TEST_TIMEOUT:-60}"
FORMAT="${TEST_FORMAT:-text}"
OUTPUT="${TEST_OUTPUT:-}"
RUN_ID="${TEST_RUN_ID:-test-$(date +%Y%m%d%H%M%S)}"
ENVIRONMENT="${TEST_ENVIRONMENT:-testnet}"
QUIET=false
FAIL_FAST=false
CASES=()

usage() {
  cat >&2 <<'EOF'
Usage: scripts/test.sh [--suite unit|smoke|all] [--base-url URL] [--timeout seconds]
       [--format text|json] [--output path] [--case ID]... [--fail-fast] [--quiet]
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --suite) SUITE="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --timeout) TIMEOUT="${2:-}"; shift 2 ;;
    --format) FORMAT="${2:-}"; shift 2 ;;
    --output) OUTPUT="${2:-}"; shift 2 ;;
    --case) CASES+=("${2:-}"); shift 2 ;;
    --environment) ENVIRONMENT="${2:-}"; shift 2 ;;
    --fail-fast) FAIL_FAST=true; shift ;;
    --keep-data) shift ;;  # smoke 不创建数据,无需保留
    --quiet) QUIET=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage; echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$SUITE" in unit|smoke|all) ;; integration|e2e) echo "ERROR: suite $SUITE is not implemented for this project" >&2; exit 2 ;; *) echo "ERROR: invalid --suite: $SUITE" >&2; exit 2 ;; esac
case "$FORMAT" in text|json) ;; junit) echo "ERROR: --format junit is not supported" >&2; exit 2 ;; *) echo "ERROR: invalid --format: $FORMAT" >&2; exit 2 ;; esac
[[ "$TIMEOUT" =~ ^[0-9]+$ ]] || { echo "ERROR: --timeout must be a number" >&2; exit 2; }
BASE_URL="${BASE_URL%/}"

START_MS=$(($(date +%s) * 1000))
STARTED_AT="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
DEADLINE=$(( $(date +%s) + TIMEOUT ))
TOTAL=0; PASSED=0; FAILED=0; SKIPPED=0
R_ID=(); R_NAME=(); R_STATUS=(); R_MS=(); R_MSG=()

say() { [[ "$QUIET" = true ]] || printf '%s\n' "$*" >&2; }

selected() {
  [[ ${#CASES[@]} -eq 0 ]] && return 0
  local c; for c in "${CASES[@]}"; do [[ "$c" == "$1" ]] && return 0; done; return 1
}

result() { # id name status ms message
  R_ID+=("$1"); R_NAME+=("$2"); R_STATUS+=("$3"); R_MS+=("$4"); R_MSG+=("$5")
  TOTAL=$((TOTAL+1))
  case "$3" in pass) PASSED=$((PASSED+1)) ;; fail) FAILED=$((FAILED+1)) ;; skip) SKIPPED=$((SKIPPED+1)) ;; esac
  local up; up="$(printf '%s' "$3" | tr '[:lower:]' '[:upper:]')"
  [[ "$FORMAT" = json || "$QUIET" = true ]] || printf '[%s] %s %s (%s ms)\n' "$up" "$1" "$2" "$4"
  [[ "$FORMAT" = json || "$3" != fail ]] || printf '       %s\n' "$5"
}

get() { curl -fsS --max-time 10 -H 'Accept: application/json' "$BASE_URL$1" 2>/dev/null; }

# 每个用例:函数返回 0 = 通过;输出一行说明到 stdout
run_case() { # id name fn
  local id="$1" name="$2" fn="$3" t0 t1 msg rc
  selected "$id" || return 0
  if [[ "$(date +%s)" -ge "$DEADLINE" ]]; then TIMED_OUT=1; return 0; fi
  t0=$(($(date +%s%N) / 1000000))
  set +e; msg="$("$fn" 2>&1)"; rc=$?; set -e
  t1=$(($(date +%s%N) / 1000000))
  if [[ "$rc" = 0 ]]; then result "$id" "$name" pass $((t1-t0)) "$msg"
  else result "$id" "$name" fail $((t1-t0)) "$msg"; [[ "$FAIL_FAST" = true ]] && STOP=1; fi
}

jget() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1], {}, {"d": d}))' "$1"; }

# ---- smoke 用例(只读) -------------------------------------------------------
smoke_height() {
  local a b
  a="$(get /height | jget 'd["height"]')" || { echo "GET /height failed"; return 1; }
  sleep 3
  b="$(get /height | jget 'd["height"]')" || { echo "GET /height failed"; return 1; }
  [[ "$b" -gt "$a" ]] || { echo "height did not advance: $a -> $b"; return 1; }
  echo "height advanced $a -> $b"
}
smoke_version() {
  local v; v="$(get /version | jget 'd["version"]')" || { echo "GET /version failed"; return 1; }
  [[ -n "$v" ]] || { echo "empty version"; return 1; }
  echo "protocol version $v"
}
smoke_supply() {
  local c; c="$(get /supply | jget 'd["conserved"]')" || { echo "GET /supply failed"; return 1; }
  [[ "$c" == "True" || "$c" == "true" ]] || { echo "supply not conserved: $c"; return 1; }
  echo "supply conserved"
}
smoke_units() {
  local n; n="$(get '/units?limit=1' | jget 'int(d["total"])')" || { echo "GET /units failed"; return 1; }
  [[ "$n" -ge 1 ]] || { echo "no knowledge units"; return 1; }
  local u; u="$(get /unit/1 | jget 'len(d["versions"])')" || { echo "GET /unit/1 failed"; return 1; }
  [[ "$u" -ge 1 ]] || { echo "unit 1 has no versions"; return 1; }
  echo "$n units; unit 1 has $u version(s)"
}
smoke_members() {
  local n; n="$(get '/members?limit=1' | jget 'int(d["total"])')" || { echo "GET /members failed"; return 1; }
  [[ "$n" -ge 1 ]] || { echo "no members"; return 1; }
  echo "$n members"
}
smoke_write_guard() {
  # 无令牌的写入必须被拒绝(401);不提交任何有效数据
  local code; code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -X POST --data-binary x "$BASE_URL/knowledge")"
  [[ "$code" == 401 ]] || { echo "unauthenticated POST /knowledge returned $code, expected 401"; return 1; }
  echo "unauthenticated write rejected (401)"
}

run_smoke() {
  command -v curl >/dev/null 2>&1 || { echo "ERROR: curl is required" >&2; exit 3; }
  command -v python3 >/dev/null 2>&1 || { echo "ERROR: python3 is required" >&2; exit 3; }
  say "smoke: $BASE_URL ($ENVIRONMENT), run_id=$RUN_ID"
  local spec
  for spec in \
      "SMOKE-CHAIN-001|chain is producing blocks|smoke_height" \
      "SMOKE-CHAIN-002|protocol version is reported|smoke_version" \
      "SMOKE-CHAIN-003|token supply is conserved|smoke_supply" \
      "SMOKE-KNOW-001|knowledge units are readable|smoke_units" \
      "SMOKE-KNOW-002|members are readable|smoke_members" \
      "SMOKE-AUTH-001|writes require a token|smoke_write_guard"; do
    [[ "${STOP:-0}" = 1 ]] && break
    IFS='|' read -r id name fn <<<"$spec"
    run_case "$id" "$name" "$fn"
  done
}

run_unit() {
  if [[ ! -f "$PROJECT_DIR/node/Cargo.toml" ]]; then
    echo "ERROR: unit tests need a source checkout (node/Cargo.toml); this looks like an installed package" >&2
    exit 3
  fi
  command -v cargo >/dev/null 2>&1 || { echo "ERROR: cargo is required for unit tests" >&2; exit 3; }
  local t0 t1 rc
  t0=$(($(date +%s%N) / 1000000))
  set +e; (cd "$PROJECT_DIR/node" && cargo test --release -- --test-threads=1) >&2; rc=$?; set -e
  t1=$(($(date +%s%N) / 1000000))
  if [[ "$rc" = 0 ]]; then result UNIT-NODE-001 "node cargo test" pass $((t1-t0)) "cargo test passed"
  else result UNIT-NODE-001 "node cargo test" fail $((t1-t0)) "cargo test failed (exit $rc)"; fi
}

TIMED_OUT=0
case "$SUITE" in
  unit) run_unit ;;
  smoke) run_smoke ;;
  all) run_unit; run_smoke ;;
esac

if [[ ${#CASES[@]} -gt 0 && "$TOTAL" -eq 0 ]]; then echo "ERROR: no test case matched --case ${CASES[*]}" >&2; exit 2; fi
if [[ "$TOTAL" -eq 0 && "$TIMED_OUT" = 0 ]]; then echo "ERROR: no test cases were run" >&2; exit 3; fi

DURATION_MS=$(( $(date +%s) * 1000 - START_MS ))
STATUS=pass; [[ "$FAILED" -eq 0 ]] || STATUS=fail

emit_json() {
  local esc i
  esc() { printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr -d '\000-\037'; }
  printf '{"schema_version":"1.0","type":"automated_test","project":"zhixing-testnet","version":"unknown",'
  printf '"environment":"%s","suite":"%s","run_id":"%s","status":"%s","started_at":"%s","duration_ms":%s,' \
    "$ENVIRONMENT" "$SUITE" "$RUN_ID" "$STATUS" "$STARTED_AT" "$DURATION_MS"
  printf '"summary":{"total":%s,"passed":%s,"failed":%s,"skipped":%s},"cases":[' "$TOTAL" "$PASSED" "$FAILED" "$SKIPPED"
  for i in "${!R_ID[@]}"; do
    [[ $i -gt 0 ]] && printf ','
    printf '{"id":"%s","name":"%s","status":"%s","duration_ms":%s,"message":"%s"}' \
      "$(esc "${R_ID[$i]}")" "$(esc "${R_NAME[$i]}")" "${R_STATUS[$i]}" "${R_MS[$i]}" "$(esc "${R_MSG[$i]}")"
  done
  printf ']}\n'
}

if [[ "$FORMAT" = json ]]; then
  if [[ -n "$OUTPUT" ]]; then emit_json > "$OUTPUT"; else emit_json; fi
else
  line="RESULT status=$STATUS total=$TOTAL passed=$PASSED failed=$FAILED skipped=$SKIPPED duration_ms=$DURATION_MS"
  [[ "$QUIET" = true ]] || printf '%s\n' "$line"
  [[ -z "$OUTPUT" ]] || printf '%s\n' "$line" > "$OUTPUT"
fi

[[ "$TIMED_OUT" = 0 ]] || exit 4
[[ "$STATUS" = pass ]] || exit 1
exit 0
