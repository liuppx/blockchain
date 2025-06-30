#!/bin/bash

# 导入通用配置
source common.sh

# 设置 Beacon Chain 配置
setup_beacon_config() {
    log_info "Setting up Beacon Chain configuration..."

    # 检查基础配置是否存在
    if [[ ! -d "$OUTPUT_DIR/data/execution/geth" ]]; then
        log_error "Get datadir not found. Please run Geth setup first."
        exit 1
    fi

    USER_ADDRESS=$(get_user_address)
    RECIPIENT_ADDRESS=$(get_recipient_address)
    WITHDRAWAL_ADDRESS=$(get_withdrawal_address)
    MNEMONICS=$(get_node_mnemonics)

    # 确保配置已设置
    if [[ ! -f "$OUTPUT_DIR/data/consensus/config.yaml" ]]; then
        log_info "Beacon Chain not configured, setting up now..."
        create_consensus_config
    fi

    if [[ ! -f "$OUTPUT_DIR/data/consensus/genesis.ssz" ]]; then
        create_consensus_genesis_state
    fi
}

# 创建共识层创世配置
create_consensus_config() {
    log_info "Creating consensus layer genesis..."
    if [[ ! -f "$OUTPUT_DIR/config/genesis_timestamp.txt" ]]; then
        log_error "Genesis timestamp not found. Please run Geth setup first."
        exit 1
    fi

    # 读取创世时间戳
    GENESIS_TIMESTAMP=$(cat $OUTPUT_DIR/config/genesis_timestamp.txt)

    cat >$OUTPUT_DIR/data/consensus/config.yaml <<EOF
# Electra 测试网络配置
# 有两个主要选项：
# mainnet - 主网配置
# minimal - 最小化配置（用于测试）
PRESET_BASE: 'mainnet'

CONFIG_NAME: '${NETWORK_NAME}'

# Transition
# ---------------------------------------------------------------
# Estimated on Sept 15, 2022
TERMINAL_TOTAL_DIFFICULTY: 0
# By default, don't use these params
TERMINAL_BLOCK_HASH: 0x0000000000000000000000000000000000000000000000000000000000000000
TERMINAL_BLOCK_HASH_ACTIVATION_EPOCH: 18446744073709551615

# Genesis
# ---------------------------------------------------------------
# 2**14 (= 16,384)
MIN_GENESIS_ACTIVE_VALIDATOR_COUNT: 4
# 1970-Jan-01 12:00:00 AM UTC
MIN_GENESIS_TIME: ${GENESIS_TIMESTAMP}
# Mainnet initial fork version, recommend altering for testnets
GENESIS_FORK_VERSION: 0x20000001
# Some seconds
GENESIS_DELAY: 0

# Forking
# ---------------------------------------------------------------
# Some forks are disabled for now:
#  - These may be re-assigned to another fork-version later
#  - Temporarily set to max uint64 value: 2**64 - 1

# Altair
ALTAIR_FORK_VERSION: 0x20010001
ALTAIR_FORK_EPOCH: 0
# Merge
BELLATRIX_FORK_VERSION: 0x20020001
BELLATRIX_FORK_EPOCH: 0
# Capella
CAPELLA_FORK_VERSION: 0x20030001
CAPELLA_FORK_EPOCH: 0
# Deneb
DENEB_FORK_VERSION: 0x20040001
DENEB_FORK_EPOCH: 0
# Electra
ELECTRA_FORK_VERSION: 0x20050001
ELECTRA_FORK_EPOCH: 0
# Fulu
FULU_FORK_VERSION: 0x20060001
# 使用最大值表示"永不激活"
FULU_FORK_EPOCH: 18446744073709551615

# Time parameters
# ---------------------------------------------------------------
# 12 seconds
SECONDS_PER_SLOT: 12
# 14 (estimate from Eth1 mainnet)
SECONDS_PER_ETH1_BLOCK: 12
# 2**8 (= 256) epochs ~27 hours
MIN_VALIDATOR_WITHDRAWABILITY_DELAY: 256
# 2**8 (= 256) epochs ~27 hours
SHARD_COMMITTEE_PERIOD: 256
# 2**11 (= 2,048) Eth1 blocks ~8 hours
ETH1_FOLLOW_DISTANCE: 2048


# Validator cycle
# ---------------------------------------------------------------
# 2**2 (= 4)
INACTIVITY_SCORE_BIAS: 4
# 2**4 (= 16)
INACTIVITY_SCORE_RECOVERY_RATE: 16
# 2**4 * 10**9 (= 16,000,000,000) Gwei
EJECTION_BALANCE: 16000000000
# 2**2 (= 4)
MIN_PER_EPOCH_CHURN_LIMIT: 4
# 2**16 (= 65,536)
CHURN_LIMIT_QUOTIENT: 65536
# [New in Deneb:EIP7514] 2**3 (= 8)
MAX_PER_EPOCH_ACTIVATION_CHURN_LIMIT: 8

# Fork choice
# ---------------------------------------------------------------
# 40%
PROPOSER_SCORE_BOOST: 40
# 20%
REORG_HEAD_WEIGHT_THRESHOLD: 20
# 160%
REORG_PARENT_WEIGHT_THRESHOLD: 160
# 2 epochs
REORG_MAX_EPOCHS_SINCE_FINALIZATION: 2

# Deposit contract
# ---------------------------------------------------------------
DEPOSIT_CHAIN_ID: ${CHAIN_ID}
DEPOSIT_NETWORK_ID: ${CHAIN_ID}
DEPOSIT_CONTRACT_ADDRESS: 0x4242424242424242424242424242424242424242


# Networking
# ---------------------------------------------------------------
# 10 * 2**20 (= 10485760, 10 MiB)
MAX_PAYLOAD_SIZE: 10485760
# 2**10 (= 1024)
MAX_REQUEST_BLOCKS: 1024
# 2**8 (= 256)
EPOCHS_PER_SUBNET_SUBSCRIPTION: 256
# MIN_VALIDATOR_WITHDRAWABILITY_DELAY + CHURN_LIMIT_QUOTIENT // 2 (= 33024, ~5 months)
MIN_EPOCHS_FOR_BLOCK_REQUESTS: 33024
# 5s
TTFB_TIMEOUT: 5
# 10s
RESP_TIMEOUT: 10
ATTESTATION_PROPAGATION_SLOT_RANGE: 32
# 500ms
MAXIMUM_GOSSIP_CLOCK_DISPARITY: 500
MESSAGE_DOMAIN_INVALID_SNAPPY: 0x00000000
MESSAGE_DOMAIN_VALID_SNAPPY: 0x01000000
# 2 subnets per node
SUBNETS_PER_NODE: 2
# 2**8 (= 64)
ATTESTATION_SUBNET_COUNT: 64
ATTESTATION_SUBNET_EXTRA_BITS: 0
# ceillog2(ATTESTATION_SUBNET_COUNT) + ATTESTATION_SUBNET_EXTRA_BITS
ATTESTATION_SUBNET_PREFIX_BITS: 6

# Deneb
# 2**7 (=128)
MAX_REQUEST_BLOCKS_DENEB: 128
# 2**12 (= 4096 epochs, ~18 days)
MIN_EPOCHS_FOR_BLOB_SIDECARS_REQUESTS: 4096
# 6
BLOB_SIDECAR_SUBNET_COUNT: 6
## uint64(6)
MAX_BLOBS_PER_BLOCK: 6
# MAX_REQUEST_BLOCKS_DENEB * MAX_BLOBS_PER_BLOCK
MAX_REQUEST_BLOB_SIDECARS: 768

# Electra
# 2**7 * 10**9 (= 128,000,000,000)
MIN_PER_EPOCH_CHURN_LIMIT_ELECTRA: 128000000000
# 2**8 * 10**9 (= 256,000,000,000)
MAX_PER_EPOCH_ACTIVATION_EXIT_CHURN_LIMIT: 256000000000
# 9
BLOB_SIDECAR_SUBNET_COUNT_ELECTRA: 9
# uint64(9)
MAX_BLOBS_PER_BLOCK_ELECTRA: 9
# MAX_REQUEST_BLOCKS_DENEB * MAX_BLOBS_PER_BLOCK_ELECTRA
MAX_REQUEST_BLOB_SIDECARS_ELECTRA: 1152
EOF
}

# 创建共识层创世状态
create_consensus_genesis_state() {
    log_info "Generating genesis state...\n"
    # 生成验证者助记词
    log_info "Generating validator mnemonics..."

    cat >$OUTPUT_DIR/data/consensus/mnemonics.yaml <<EOF
- mnemonic: "${MNEMONICS}"                                 # a 24 word BIP 39 mnemonic
  start: 0                                                 # account index to start from
  count: ${VALIDATOR_COUNT}                                # number of validators to generate
  balance: 32000000000                                     # effective balance
  wd_address: "${WITHDRAWAL_ADDRESS}"                      # withdrawal address
  wd_prefix: "0x02"                                        # withdrawal credentials prefix
EOF

    eth-beacon-genesis devnet \
        --eth1-config $OUTPUT_DIR/data/execution/genesis.json \
        --config $OUTPUT_DIR/data/consensus/config.yaml \
        --mnemonics $OUTPUT_DIR/data/consensus/mnemonics.yaml \
        --state-output $OUTPUT_DIR/data/consensus/genesis.ssz
}

# 启动 Beacon Chain
start_beacon() {
    log_header "[START] Starting Beacon Chain consensus client..."
    setup_beacon_config

    # 检查 Geth 是否运行
    if ! curl -s -X POST -H "Content-Type: application/json" --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' http://localhost:8545 >/dev/null; then
        log_error "Geth is not running. Please start Geth first."
        exit 1
    fi

    BEACON_BOOTNODE=""
    if [ -f "$OUTPUT_DIR/config/beacon_enr.txt" ]; then
        BEACON_BOOTNODE="--bootstrap-node $(cat $OUTPUT_DIR/config/beacon_enr.txt)"
    fi

    local beacon_cmd="beacon-chain \
        --datadir $OUTPUT_DIR/data/consensus \
        --min-sync-peers 0 \
        --genesis-state $OUTPUT_DIR/data/consensus/genesis.ssz \
        --bootstrap-node= \
        --chain-config-file $OUTPUT_DIR/data/consensus/config.yaml \
        --contract-deployment-block 0 \
        --chain-id $CHAIN_ID \
        --network-id $CHAIN_ID \
        --rpc-host 0.0.0.0 \
        --rpc-port 4000 \
        --grpc-gateway-host 0.0.0.0 \
        --grpc-gateway-port 3500 \
        --p2p-tcp-port=13000 \
        --p2p-udp-port=12000 \
        $BEACON_BOOTNODE \
        --execution-endpoint http://localhost:8551 \
        --accept-terms-of-use \
        --jwt-secret $OUTPUT_DIR/config/jwt.hex \
        --suggested-fee-recipient ${RECIPIENT_ADDRESS} \
        --minimum-peers-per-subnet 0 \
        --log-file $OUTPUT_DIR/logs/beacon.log \
        --verbosity debug"

    if [[ "$1" == "background" ]]; then
        eval "$beacon_cmd" >$OUTPUT_DIR/logs/beacon_cmd.log 2>&1 &
        echo $! >$OUTPUT_DIR/.beacon.pid
        log_info "Beacon Chain started in background (PID: $(cat $OUTPUT_DIR/.beacon.pid))"

        # 等待 Beacon Chain 就绪
        wait_for_service "Beacon Chain" "curl -s http://localhost:3500/eth/v1/node/health" 120 2

        # 启动信标链后检查验证者
        LENGTH=$(curl -s "http://localhost:3500/eth/v1/beacon/states/head/validators" | jq '.data | length' 2>/dev/null)
        log_info "Active validators: $LENGTH"
    else
        log_warn "Starting Beacon Chain in foreground mode..."
        log_warn "Press Ctrl+C to stop all services"

        # 设置信号处理
        trap 'printf "\n${YELLOW}[STOP]${NC} Stopping all services...\n"; stop_services; exit 0' INT TERM

        eval "$beacon_cmd"
    fi

    if [ ! -f "$OUTPUT_DIR/config/beacon_enr.txt" ]; then
        log_info "🔍 Getting Beacon Chain ENR..."
        for i in {1..10}; do
            if curl -s http://localhost:3500/eth/v1/node/identity > /tmp/node_identity.json 2>/dev/null; then
                ENR=$(jq -r '.data.enr' /tmp/node_identity.json 2>/dev/null)
                if [ -n "$ENR" ] && [ "$ENR" != "null" ]; then
                    log_info "Beacon ENR: $ENR"
                    echo "$ENR" > $OUTPUT_DIR/config/beacon_enr.txt
                    break
                fi
            fi
            log_info "Waiting Beacon ENR... ($i/10)"
            sleep 3
        done
    fi
}

# 停止 Beacon Chain
stop_beacon() {
    if [[ -f "$OUTPUT_DIR/.beacon.pid" ]]; then
        local pid=$(cat $OUTPUT_DIR/.beacon.pid)
        if kill -0 $pid 2>/dev/null; then
            log_info "Stopping Beacon Chain (PID: $pid)..."
            kill $pid
            rm -f $OUTPUT_DIR/.beacon.pid
        fi
    fi
}

# 如果直接运行此脚本
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    case "${1:-}" in
    "start")
        start_beacon "${2:-}"
        ;;
    "stop")
        stop_beacon
        ;;
    "setup")
        setup_beacon_config
        ;;
    *)
        echo "Usage: $0 {start|stop|setup} [background]"
        exit 1
        ;;
    esac
fi
