#!/bin/bash

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m' # No Color

# 配置参数
NETWORK_NAME=${NETWORK_NAME:-DevNet}
CHAIN_ID=${CHAIN_ID:-5432}
VALIDATOR_COUNT=4
PASSWORD=${PASSWORD:-Aa123456}
GENESIS_DELAY=30
OUTPUT_DIR="$HOME/.network/$NETWORK_NAME"
mkdir -p $OUTPUT_DIR

# 从env文件加载NAT_IP配置，默认为127.0.0.1
if [ -f .env ]; then
    source .env
fi

NAT_IP=${NAT_IP:-127.0.0.1}

# 通用函数
log_info() {
    printf "${GREEN}[INFO]${NC} $1\n"
}

log_error() {
    printf "${RED}[ERROR]${NC} $1\n"
}

log_warn() {
    printf "${YELLOW}[WARN]${NC} $1\n"
}

log_debug() {
    printf "${BLUE}[DEBUG]${NC} $1\n"
}

log_header() {
    printf "${CYAN}$1${NC}\n"
}

# 检查依赖
check_dependencies() {
    log_info "Checking dependencies..."

    local missing_deps=()

    # 检查必要的命令
    for cmd in geth openssl curl python3; do
        if ! command -v $cmd &>/dev/null; then
            missing_deps+=($cmd)
        fi
    done

    if [[ ${#missing_deps[@]} -ne 0 ]]; then
        log_error "Missing dependencies: ${missing_deps[*]}"
        printf "Please install the missing dependencies and try again.\n"
        exit 1
    fi

    log_info "All dependencies are available."
}

# 等待服务就绪
wait_for_service() {
    local service_name=$1
    local check_command=$2
    local max_retries=${3:-30}
    local retry_interval=${4:-1}

    log_info "Waiting for $service_name to be ready..."

    local retries=0
    while ! eval "$check_command" >/dev/null 2>&1; do
        if [[ $retries -ge $max_retries ]]; then
            log_error "$service_name failed to start within $max_retries seconds"
            return 1
        fi

        sleep $retry_interval
        ((retries++))
        printf "."
    done

    printf "\n"
    log_info "$service_name is ready."
    return 0
}

# 停止服务
stop_services() {
    log_warn "Stopping existing services..."

    # 查找并终止相关进程
    pkill -f "geth.*datadir.*data/execution" || true
    pkill -f "beacon-chain.*datadir.*data/consensus" || true
    pkill -f "validator.*datadir.*data/validator" || true

    # 清理 PID 文件
    rm -f $OUTPUT_DIR/.geth.pid $OUTPUT_DIR/.beacon.pid $OUTPUT_DIR/.validator.pid

    sleep 2
    log_info "Services stopped."
}

# 创建目录结构
create_directories() {
    log_info "Creating directory structure..."
    mkdir -p $OUTPUT_DIR/data/{execution,consensus,validator}
    mkdir -p $OUTPUT_DIR/config
    mkdir -p $OUTPUT_DIR/logs
    mkdir -p $OUTPUT_DIR/accounts
}

# 清理数据
clean_data() {
    log_info "Removing existing data..."

    # 清理数据
    rm -rf $OUTPUT_DIR/data
    # 清理账户
    rm -rf $OUTPUT_DIR/accounts
    # 清理日志
    rm -rf $OUTPUT_DIR/logs
    # 清理配置
    rm -rf $OUTPUT_DIR/config

    log_info "Data cleaned."
}

# 获取用户地址
get_user_address() {
    if [[ -f "$OUTPUT_DIR/config/user_address.txt" ]]; then
        cat $OUTPUT_DIR/config/user_address.txt
    else
        log_error "Genesis user address file not found: $OUTPUT_DIR/config/user_address.txt"
        exit 1
    fi
}

get_recipient_address() {
    if [[ -f "$OUTPUT_DIR/config/recipient_address.txt" ]]; then
        cat $OUTPUT_DIR/config/recipient_address.txt
    else
        log_error "Recipient address file not found: $OUTPUT_DIR/config/recipient_address.txt"
        exit 1
    fi
}

get_withdrawal_address() {
    if [[ -f "$OUTPUT_DIR/config/withdrawal_address.txt" ]]; then
        cat $OUTPUT_DIR/config/withdrawal_address.txt
    else
        log_error "Withdrawal address file not found: $OUTPUT_DIR/config/withdrawal_address.txt"
        exit 1
    fi
}

get_node_mnemonics() {
    if [[ -f "$OUTPUT_DIR/config/mnemonics.txt" ]]; then
        cat $OUTPUT_DIR/config/mnemonics.txt
    else
        log_error "Validator mnemonics file not found: $OUTPUT_DIR/config/mnemonics.txt"
        exit 1
    fi
}

# 检测操作系统
detect_os() {
    if [[ "$OSTYPE" == "linux-gnu"* ]]; then
        echo "linux"
    elif [[ "$OSTYPE" == "darwin"* ]]; then
        echo "macos"
    else
        echo "unknown"
    fi
}

OS=$(detect_os)
