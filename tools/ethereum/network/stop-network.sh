#!/bin/bash

# 导入通用配置
source common.sh

# 主函数
main() {
    log_header "=== Stopping Ethereum PoS Private Network ==="

    # 停止 Validator
    log_info "Stopping Validator client..."
    bash ./validator-service.sh stop

    # 停止 Beacon Chain
    log_info "Stopping Beacon Chain consensus client..."
    bash ./beacon-service.sh stop

    # 停止 Geth
    log_info "Stopping Geth execution client..."
    bash ./geth-service.sh stop

    # 强制清理残留进程
    cleanup_processes

    log_info "✅ All services stopped successfully!"
}

# 清理进程
cleanup_processes() {
    log_info "Cleaning up any remaining processes..."
    
    # 查找并终止相关进程
    local geth_pids=$(pgrep -f "geth.*--datadir.*data/execution" 2>/dev/null || true)
    local beacon_pids=$(pgrep -f "beacon-chain.*--datadir.*data/consensus" 2>/dev/null || true)
    local validator_pids=$(pgrep -f "validator.*--datadir.*data/validator" 2>/dev/null || true)
    
    # 终止 Geth 进程
    if [[ -n "$geth_pids" ]]; then
        log_info "Terminating Geth processes: $geth_pids"
        echo "$geth_pids" | xargs -r kill -TERM 2>/dev/null || true
        sleep 2
        echo "$geth_pids" | xargs -r kill -KILL 2>/dev/null || true
    fi
    
    # 终止 Beacon Chain 进程
    if [[ -n "$beacon_pids" ]]; then
        log_info "Terminating Beacon Chain processes: $beacon_pids"
        echo "$beacon_pids" | xargs -r kill -TERM 2>/dev/null || true
        sleep 2
        echo "$beacon_pids" | xargs -r kill -KILL 2>/dev/null || true
    fi
    
    # 终止 Validator 进程
    if [[ -n "$validator_pids" ]]; then
        log_info "Terminating Validator processes: $validator_pids"
        echo "$validator_pids" | xargs -r kill -TERM 2>/dev/null || true
        sleep 2
        echo "$validator_pids" | xargs -r kill -KILL 2>/dev/null || true
    fi
    
    # 清理 PID 文件
    rm -f $OUTPUT_DIR/.geth.pid $OUTPUT_DIR/.beacon.pid $OUTPUT_DIR/.validator.pid
    
    log_info "Process cleanup completed"
}

# 显示清理选项
show_cleanup_options() {
    printf "\n${CYAN}Additional cleanup options:${NC}\n"
    printf "  Clean all data: ./scripts/clean.sh\n"
    printf "  Clean logs only: rm -rf logs/*\n"
    printf "  Reset network: ./scripts/clean.sh && ./setup-config.sh\n"
    printf "\n"
}

# 如果直接运行此脚本
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    case "${1:-stop}" in
        "stop"|"")
            main
            show_cleanup_options
            ;;
        "force")
            log_info "Force stopping all services..."
            cleanup_processes
            ;;
        *)
            echo "Usage: $0 {stop|force}"
            echo "  stop   - Gracefully stop all services (default)"
            echo "  force  - Force kill all processes"
            exit 1
            ;;
    esac
fi
