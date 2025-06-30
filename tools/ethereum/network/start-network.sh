#!/bin/bash

set -e

# 导入通用配置
source common.sh

# 主函数
main() {
    log_header "=== Starting Ethereum PoS Private Network ==="

    # 检查依赖
    check_dependencies

    # 停止现有服务
    stop_services

    # 检查基础配置
    if [[ ! -f "$OUTPUT_DIR/config/jwt.hex" ]] || [[ ! -f "$OUTPUT_DIR/config/user_address.txt" ]]; then
        log_info "Basic configuration not found, running setup..."
        bash ./setup-config.sh
    fi

    # 启动 Geth
    log_info "Starting Geth execution client..."
    bash ./geth-service.sh start background

    # 启动 Beacon Chain
    log_info "Starting Beacon Chain consensus client..."
    bash ./beacon-service.sh start background

    # 启动 Validator
    log_info "Starting Validator client..."
    bash ./validator-service.sh start background

    # 显示状态
    show_network_status

    log_info "✅ All services started successfully!"
    log_info "Network is ready for use."
    
    # 显示使用说明
    show_usage_info
}

# 显示网络状态
show_network_status() {
    log_header "=== Network Status ==="
    
    printf "${CYAN}Network Information:${NC}\n"
    printf "  Network Name: %s\n" "$NETWORK_DISPLAY_NAME"
    printf "  Chain ID: %s\n" "$CHAIN_ID"
    printf "  Validators: %s\n" "$VALIDATOR_COUNT"
    printf "\n"

    printf "${CYAN}Service Endpoints:${NC}\n"
    printf "  Geth HTTP RPC: http://localhost:8545\n"
    printf "  Geth WebSocket: ws://localhost:8546\n"
    printf "  Beacon Chain API: http://localhost:3500\n"
    printf "  Beacon Chain RPC: http://localhost:4000\n"
    printf "\n"

    printf "${CYAN}Monitoring:${NC}\n"
    printf "  Geth Metrics: http://localhost:6060/debug/metrics\n"
    printf "  Beacon Metrics: http://localhost:8080/metrics\n"
    printf "  Validator Metrics: http://localhost:8081/metrics\n"
    printf "\n"

    printf "${CYAN}Log Files:${NC}\n"
    printf "  Geth: logs/geth.log\n"
    printf "  Beacon Chain: logs/beacon.log\n"
    printf "  Validator: logs/validator.log\n"
    printf "\n"
}

# 显示使用说明
show_usage_info() {
    log_header "=== Usage Information ==="
    
    printf "${GREEN}Network Management:${NC}\n"
    printf "  Stop network: ./scripts/stop-network.sh\n"
    printf "  View logs: tail -f logs/geth.log\n"
    printf "  Check status: ./scripts/status.sh\n"
    printf "\n"

    printf "${GREEN}Connect to Network:${NC}\n"
    printf "  RPC URL: http://localhost:8545\n"
    printf "  Chain ID: %s\n" "$CHAIN_ID"
    printf "  Example: curl -X POST -H \"Content-Type: application/json\" \\\\\n"
    printf "           --data '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}' \\\\\n"
    printf "           http://localhost:8545\n"
    printf "\n"

    if [[ -f "$OUTPUT_DIR/config/user_address.txt" ]] && [[ -f "$OUTPUT_DIR/config/genesis_node.txt" ]]; then
        printf "${GREEN}Pre-funded Account:${NC}\n"
        printf "  Address: %s\n" "$(cat $OUTPUT_DIR/config/user_address.txt)"
        printf "  Balance: 5000000 ETH (pre-funded)\n"
        printf "\n"
    fi

    printf "${YELLOW}Note:${NC} All services are running in background mode.\n"
    printf "Use 'ps aux | grep -E \"geth|beacon-chain|validator\"' to see running processes.\n"
}

# 检查依赖
check_dependencies() {
    log_info "Checking dependencies..."
    
    local missing_deps=()
    
    if ! command -v geth &>/dev/null; then
        missing_deps+=("geth")
    fi
    
    if ! command -v beacon-chain &>/dev/null; then
        missing_deps+=("beacon-chain (Prysm)")
    fi
    
    if ! command -v validator &>/dev/null; then
        missing_deps+=("validator (Prysm)")
    fi
    
    if [[ ${#missing_deps[@]} -gt 0 ]]; then
        log_error "Missing dependencies: ${missing_deps[*]}"
        log_error "Please install the required Ethereum clients:"
        log_error "  - Geth: https://geth.ethereum.org/downloads/"
        log_error "  - Prysm: https://docs.prylabs.network/docs/install/install-with-script"
        exit 1
    fi
    
    log_info "All dependencies found ✓"
}

# 停止所有服务
stop_services() {
    log_info "Stopping any existing services..."
    
    # 停止各个服务
    bash ./validator-service.sh stop 2>/dev/null || true
    bash ./beacon-service.sh stop 2>/dev/null || true
    bash ./geth-service.sh stop 2>/dev/null || true
    
    # 等待进程完全停止
    sleep 2
    
    # 强制清理残留进程
    pkill -f "geth.*--datadir.*data/execution" 2>/dev/null || true
    pkill -f "beacon-chain.*--datadir.*data/consensus" 2>/dev/null || true
    pkill -f "validator.*--datadir.*data/validator" 2>/dev/null || true

    # 清理 PID 文件
    rm -f $OUTPUT_DIR/.geth.pid $OUTPUT_DIR/.beacon.pid $OUTPUT_DIR/.validator.pid
    
    log_info "Services stopped"
}

# 信号处理
cleanup() {
    printf "\n${YELLOW}[STOP]${NC} Received interrupt signal, stopping services...\n"
    stop_services
    exit 0
}

# 设置信号处理
trap cleanup INT TERM

# 如果直接运行此脚本
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    case "${1:-start}" in
        "start"|"")
            main
            ;;
        "stop")
            stop_services
            ;;
        "restart")
            stop_services
            sleep 3
            main
            ;;
        *)
            echo "Usage: \$0 {start|stop|restart}"
            echo "  start    - Start the network (default)"
            echo "  stop     - Stop all services"
            echo "  restart  - Restart the network"
            exit 1
            ;;
    esac
fi
