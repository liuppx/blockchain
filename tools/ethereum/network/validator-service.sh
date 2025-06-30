#!/bin/bash

# 导入通用配置
source common.sh

# 设置 Validator 配置
setup_validator_config() {
    log_info "Setting up Validator configuration..."
    
    RECIPIENT_ADDRESS=$(get_recipient_address)
    MNEMONICS=$(get_node_mnemonics)

    # 检查验证者密钥是否存在
    if [[ ! -d "$OUTPUT_DIR/data/validator/wallet" ]] || [[ -z "$(ls -A $OUTPUT_DIR/data/validator/wallet 2>/dev/null)" ]]; then
        log_info "Prysm validator not configured, setting up now..."
        generate_validator_keys
    fi
}

# 生成验证者密钥
generate_validator_keys() {
    log_info "Generating validator keys...\n"

    eth2-val-tools keystores \
        --source-mnemonic "${MNEMONICS}" \
        --source-min 0 \
        --source-max 4 \
        --out-loc $OUTPUT_DIR/data/validator/keys \
        --prysm-pass "${PASSWORD}" \
        --insecure

    log_info "Checking validator keys..."

    mv $OUTPUT_DIR/data/validator/keys/prysm $OUTPUT_DIR/data/validator/wallet

    # 验证密钥导入
    validator accounts list \
        --wallet-password-file $OUTPUT_DIR/config/password.txt \
        --wallet-dir $OUTPUT_DIR/data/validator/wallet \
        --accept-terms-of-use

    log_info "Validator keys setup completed!"
}


# 启动 Validator
start_validator() {
    log_header "[START] Starting Validator client..."
    setup_validator_config

    # 检查 Beacon Chain 是否运行
    if ! curl -s http://localhost:3500/eth/v1/node/health >/dev/null; then
        log_error "Beacon Chain is not running. Please start Beacon Chain first."
        exit 1
    fi

    local validator_cmd="validator \
        --datadir $OUTPUT_DIR/data/validator \
        --accept-terms-of-use \
        --wallet-dir $OUTPUT_DIR/data/validator/wallet \
        --wallet-password-file $OUTPUT_DIR/config/password.txt \
        --chain-config-file $OUTPUT_DIR/data/consensus/config.yaml \
        --beacon-rpc-provider localhost:4000 \
        --suggested-fee-recipient ${RECIPIENT_ADDRESS} \
        --log-file $OUTPUT_DIR/logs/validator.log \
        --verbosity debug"

    if [[ "$1" == "background" ]]; then
        eval "$validator_cmd" >$OUTPUT_DIR/logs/validator_cmd.log 2>&1 &
        echo $! >$OUTPUT_DIR/.validator.pid
        log_info "Validator started in background (PID: $(cat $OUTPUT_DIR/.validator.pid))"

        # 等待 Validator 就绪
        wait_for_service "Validator" "curl -s http://localhost:8081/metrics | grep -q validator" 30 2
    else
        log_warn "Starting Validator in foreground mode..."
        log_warn "Press Ctrl+C to stop all services"

        # 设置信号处理
        trap 'printf "\n${YELLOW}[STOP]${NC} Stopping all services...\n"; stop_services; exit 0' INT TERM

        eval "$validator_cmd"
    fi
}

# 停止 Validator
stop_validator() {
    if [[ -f "$OUTPUT_DIR/.validator.pid" ]]; then
        local pid=$(cat $OUTPUT_DIR/.validator.pid)
        if kill -0 $pid 2>/dev/null; then
            log_info "Stopping Validator (PID: $pid)..."
            kill $pid
            rm -f $OUTPUT_DIR/.validator.pid
        fi
    fi
}

# 如果直接运行此脚本
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    case "${1:-}" in
        "start")
            start_validator "${2:-}"
            ;;
        "stop")
            stop_validator
            ;;
        "setup")
            setup_validator_config
            ;;
        *)
            echo "Usage: $0 {start|stop|setup} [background]"
            exit 1
            ;;
    esac
fi
