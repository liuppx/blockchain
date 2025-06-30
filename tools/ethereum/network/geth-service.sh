#!/bin/bash

# 导入通用配置
source common.sh

# 设置 Geth 配置
setup_geth_config() {
    log_info "Setting up Geth configuration..."

    # 检查基础配置是否存在
    if [[ ! -f "$OUTPUT_DIR/config/user_address.txt" ]]; then
        log_error "Basic configuration not found. Please run './setup-config.sh' first."
        exit 1
    fi

    # 读取账户地址
    USER_ADDRESS=$(get_user_address)

    # 确保配置已设置
    if [[ ! -f "$OUTPUT_DIR/data/execution/genesis.json" ]]; then
        log_info "Geth not configured, setting up now..."
        generate_genesis_time
        create_execution_genesis
    fi

    if [[ ! -d "$OUTPUT_DIR/data/execution/geth" ]]; then
        log_info "The geth datadir not found, initialize now..."
        init_geth_datadir
    fi
}

# 生成统一的创世时间戳（确保所有组件使用相同时间）
generate_genesis_time() {
    # 生成未来30秒的时间戳，给配置生成留出时间
    local GENESIS_TIMESTAMP=$(($(date +%s) + ${GENESIS_DELAY}))

    echo "${GENESIS_TIMESTAMP}" >$OUTPUT_DIR/config/genesis_timestamp.txt
    if [[ "$OS" == "macos" ]]; then
        log_info "统一创世时间戳: ${GENESIS_TIMESTAMP} ($(date -r ${GENESIS_TIMESTAMP}))"
    elif [[ "$OS" == "linux" ]]; then
        log_info "统一创世时间戳: ${GENESIS_TIMESTAMP} ($(date -d @${GENESIS_TIMESTAMP}))"
    fi
}

# 创建执行层创世配置
create_execution_genesis() {
    log_info "Creating execution layer genesis..."

    # 生成统一的时间戳
    local GENESIS_TIMESTAMP=$(cat $OUTPUT_DIR/config/genesis_timestamp.txt)
    local GENESIS_TIMESTAMP_HEX=$(printf "0x%x" $GENESIS_TIMESTAMP)
    echo "1" >$OUTPUT_DIR/config/genesis_node.txt
    cat >$OUTPUT_DIR/data/execution/genesis.json <<EOF
{
  "config": {
    "chainId": ${CHAIN_ID},
    "homesteadBlock": 0,
    "eip150Block": 0,
    "eip155Block": 0,
    "eip158Block": 0,
    "byzantiumBlock": 0,
    "constantinopleBlock": 0,
    "petersburgBlock": 0,
    "istanbulBlock": 0,
    "berlinBlock": 0,
    "londonBlock": 0,
    "mergeNetsplitBlock": 0,
    "terminalTotalDifficulty": 0,
    "terminalTotalDifficultyPassed": true,
    "shanghaiTime": 0,
    "cancunTime": 0,
    "blobSchedule": {
      "cancun": {
        "target": 3,
        "max": 6,
        "baseFeeUpdateFraction": 3338477
      },
      "prague": {
        "target": 6,
        "max": 9,
        "baseFeeUpdateFraction": 5007716
      }
    },
    "depositContractAddress": "0x4242424242424242424242424242424242424242",
    "pragueTime": 0
  },
  "coinbase": "0x0000000000000000000000000000000000000000",
  "difficulty": "0x0",
  "extraData": "",
  "gasLimit": "0x2255100",
  "nonce": "0x1234",
  "mixhash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
  "timestamp": "${GENESIS_TIMESTAMP_HEX}",
  "alloc": {
    "${USER_ADDRESS}": {
      "balance": "0x422ca8b0a00a425000000"
    }
  }
}
EOF
}

# 初始化 Geth 数据目录
init_geth_datadir() {
    log_info "Initializing Geth data directory..."
    geth --datadir $OUTPUT_DIR/data/execution init $OUTPUT_DIR/data/execution/genesis.json
    log_info "Geth initialization completed"
}

# 启动 Geth
start_geth() {
    log_header "[START] Starting Geth execution client..."
    setup_geth_config

    BOOTNODE_ENODE=""
    if [[ -f "$OUTPUT_DIR/config/enode.txt"  ]] && [[ ! -f "$OUTPUT_DIR/config/genesis_node.txt" ]]; then
        BOOTNODE_ENODE="--bootnodes $(cat $OUTPUT_DIR/config/enode.txt)"
    fi

    local geth_cmd="geth \
        --datadir $OUTPUT_DIR/data/execution \
        --keystore $OUTPUT_DIR/accounts \
        --password $OUTPUT_DIR/config/password.txt \
        --state.scheme=path \
        --identity \"Devnet\" \
        --port 30303 \
        --discovery.port 30303 \
        $BOOTNODE_ENODE \
        --http \
        --http.api eth,net,web3,engine,admin,debug,txpool \
        --http.addr 0.0.0.0 \
        --http.port 8545 \
        --http.corsdomain \"*\" \
        --ws \
        --ws.api eth,net,web3,engine,admin,debug,txpool \
        --ws.addr 0.0.0.0 \
        --ws.port 8546 \
        --ws.origins \"*\" \
        --authrpc.vhosts \"*\" \
        --authrpc.addr 0.0.0.0 \
        --authrpc.port 8551 \
        --authrpc.jwtsecret $OUTPUT_DIR/config/jwt.hex \
        --networkid $CHAIN_ID \
        --nat extip:$NAT_IP \
        --syncmode full \
        --maxpeers 21 \
        --log.file $OUTPUT_DIR/logs/geth.log \
        --verbosity 3"

    if [[ "$1" == "background" ]]; then
        eval "$geth_cmd" >$OUTPUT_DIR/logs/geth_cmd.log 2>&1 &
        echo $! >$OUTPUT_DIR/.geth.pid
        log_info "Geth started in background (PID: $(cat $OUTPUT_DIR/.geth.pid))"

        # 等待 Geth 就绪
        wait_for_service "Geth" "curl -s -X POST -H \"Content-Type: application/json\" --data '{\"jsonrpc\":\"2.0\",\"method\":\"eth_blockNumber\",\"params\":[],\"id\":1}' http://localhost:8545" 120 2
    else
        log_warn "Starting Geth in foreground mode..."
        log_warn "Press Ctrl+C to stop all services"

        # 设置信号处理
        trap 'printf "\n${YELLOW}[STOP]${NC} Stopping all services...\n"; stop_services; exit 0' INT TERM

        eval "$geth_cmd"
    fi

    if [ ! -f "$OUTPUT_DIR/config/enode.txt" ]; then
        # 获取enode信息
        log_info "🔍 Getting bootnode enode..."
        for i in {1..10}; do
            if ENODE=$(geth attach --datadir $OUTPUT_DIR/data/execution --exec "admin.nodeInfo.enode" 2>/dev/null | tr -d '"'); then
                if [ -n "$ENODE" ] && [ "$ENODE" != "null" ]; then
                    log_info "Bootnode enode: $ENODE"
                    echo "$ENODE" > $OUTPUT_DIR/config/enode.txt
                    break
                fi
            fi
            log_info "Waiting enode... ($i/10)"
            sleep 3
        done
    fi
}

# 停止 Geth
stop_geth() {
    if [[ -f "$OUTPUT_DIR/.geth.pid" ]]; then
        local pid=$(cat $OUTPUT_DIR/.geth.pid)
        if kill -0 $pid 2>/dev/null; then
            log_info "Stopping Geth (PID: $pid)..."
            kill $pid
            rm -f $OUTPUT_DIR/.geth.pid
        fi
    fi
}

# 如果直接运行此脚本
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    case "${1:-}" in
    "start")
        start_geth "${2:-}"
        ;;
    "stop")
        stop_geth
        ;;
    "setup")
        setup_geth_config
        ;;
    *)
        echo "Usage: $0 {start|stop|setup} [background]"
        exit 1
        ;;
    esac
fi
