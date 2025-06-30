#!/bin/bash

# 配置参数
RPC_URL="http://localhost:8545"
TOKEN_CONTRACT=""
WALLET_ADDRESS=""

# 函数签名
BALANCE_OF_SIG="0x70a08231"
DECIMALS_SIG="0x313ce567"

# 使用说明
usage() {
    echo "用法: $0 -t <代币合约地址> -w <钱包地址> [-r <RPC地址>]"
    echo "示例: $0 -t 0x123...abc -w 0x456...def -r http://localhost:8545"
    exit 1
}

# 解析命令行参数
while getopts "t:w:r:h" opt; do
    case $opt in
        t) TOKEN_CONTRACT="$OPTARG" ;;
        w) WALLET_ADDRESS="$OPTARG" ;;
        r) RPC_URL="$OPTARG" ;;
        h) usage ;;
        *) usage ;;
    esac
done

# 检查必需参数
if [[ -z "$TOKEN_CONTRACT" || -z "$WALLET_ADDRESS" ]]; then
    echo "错误: 缺少必需参数"
    usage
fi

# 格式化地址
format_address() {
    local addr=${1#0x}
    printf "%064s" "$addr" | tr ' ' '0'
}

# RPC调用
rpc_call() {
    local to=$1
    local data=$2
    
    curl -s -X POST \
        -H "Content-Type: application/json" \
        -d "{
            \"jsonrpc\":\"2.0\",
            \"method\":\"eth_call\",
            \"params\":[{
                \"to\":\"$to\",
                \"data\":\"$data\"
            }, \"latest\"],
            \"id\":1
        }" \
        "$RPC_URL" | grep -o '"result":"[^"]*"' | cut -d'"' -f4
}

# 查询代币精度
get_decimals() {
    local result=$(rpc_call "$TOKEN_CONTRACT" "$DECIMALS_SIG")
    
    if [[ -n "$result" && "$result" != "null" ]]; then
        printf "%d" "$result" 2>/dev/null || echo "18"
    else
        echo "18"
    fi
}

# 查询余额
get_balance() {
    local formatted_addr=$(format_address "$WALLET_ADDRESS")
    local data="${BALANCE_OF_SIG}${formatted_addr}"
    local result=$(rpc_call "$TOKEN_CONTRACT" "$data")
    
    if [[ -n "$result" && "$result" != "null" ]]; then
        printf "%d" "$result" 2>/dev/null
    else
        echo "0"
    fi
}

# 主函数
main() {
    echo "查询代币余额..."
    
    # 获取精度和余额
    local decimals=$(get_decimals)
    local balance=$(get_balance)
    
    echo "代币合约: $TOKEN_CONTRACT"
    echo "钱包地址: $WALLET_ADDRESS"
    echo "代币精度: $decimals"
    echo "原始余额: $balance"
    
    # 计算实际余额
    if command -v bc >/dev/null 2>&1 && [[ "$balance" != "0" ]]; then
        local divisor=$(echo "10^$decimals" | bc)
        local actual_balance=$(echo "scale=6; $balance / $divisor" | bc)
        echo "实际余额: $actual_balance"
    fi
}

main
