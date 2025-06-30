#!/bin/bash
# balance.sh - 查询以太坊账户余额 (修复大数处理)

# 默认配置
RPC_URL="http://localhost:8545"
ADDRESS=""

# 颜色定义
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

# 显示帮助信息
show_help() {
    echo "用法: $0 <账户地址> [RPC_URL]"
    echo ""
    echo "参数:"
    echo "  账户地址    要查询的以太坊地址 (必需)"
    echo "  RPC_URL     RPC节点地址 (可选，默认: http://localhost:8545)"
    echo ""
    echo "示例:"
    echo "  $0 0x742d35Cc6634C0532925a3b8D4C9db96c4b4d8e8"
    echo "  $0 0x742d35Cc6634C0532925a3b8D4C9db96c4b4d8e8 http://localhost:8545"
}

# 检查依赖
check_dependencies() {
    if ! command -v curl &> /dev/null; then
        echo -e "${RED}错误: 需要安装 curl${NC}"
        exit 1
    fi
    
    if ! command -v bc &> /dev/null; then
        echo -e "${RED}错误: 需要安装 bc${NC}"
        echo "macOS安装命令: brew install bc"
        exit 1
    fi
}

# 验证地址格式
validate_address() {
    local addr=$1
    if [[ ! $addr =~ ^0x[a-fA-F0-9]{40}$ ]]; then
        echo -e "${RED}错误: 无效的以太坊地址格式${NC}"
        echo "地址应该是42位字符，以0x开头"
        exit 1
    fi
}

# 转换为大写 (兼容版本)
to_upper() {
    echo "$1" | tr '[:lower:]' '[:upper:]'
}

# 十六进制转十进制 (使用bc处理大数)
hex_to_dec() {
    local hex_value="$1"
    # 移除0x前缀
    hex_value=${hex_value#0x}
    
    # 检查是否为空或无效
    if [ -z "$hex_value" ]; then
        echo "0"
        return
    fi
    
    # 转换为大写并使用bc进行十六进制转十进制
    local hex_upper=$(to_upper "$hex_value")
    echo "ibase=16; $hex_upper" | bc
}

# Wei转ETH (使用bc处理大数除法)
wei_to_eth() {
    local wei_value="$1"
    
    # 检查输入是否为空或无效
    if [ -z "$wei_value" ] || [ "$wei_value" = "0" ]; then
        echo "0"
        return
    fi
    
    # 1 ETH = 10^18 Wei，使用bc进行精确计算
    echo "scale=18; $wei_value / 1000000000000000000" | bc -l
}

# 格式化ETH显示
format_eth() {
    local eth_value="$1"
    
    if [ -z "$eth_value" ]; then
        echo "0"
        return
    fi
    
    # 保留6位小数，移除尾随零
    printf "%.6f" "$eth_value" | sed 's/\.0*$//' | sed 's/0*$//' | sed 's/\.$//'
}

# 获取余额
get_balance() {
    local address="$1"
    local rpc_url="$2"
    
    # 构造JSON请求
    local json_data="{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBalance\",\"params\":[\"$address\",\"latest\"],\"id\":1}"
    
    # 发送请求并获取响应
    local response=$(curl -s -X POST -H "Content-Type: application/json" \
        --data "$json_data" "$rpc_url" 2>/dev/null)
    
    if [ $? -ne 0 ]; then
        echo -e "${RED}错误: 无法连接到RPC节点${NC}" >&2
        return 1
    fi
    
    # 检查响应是否为空
    if [ -z "$response" ]; then
        echo -e "${RED}错误: RPC节点无响应${NC}" >&2
        return 1
    fi
    
    # 提取余额
    local balance_hex=$(echo "$response" | sed -n 's/.*"result":"\([^"]*\)".*/\1/p')
    
    # 如果第一种方法失败，尝试其他方法
    if [ -z "$balance_hex" ]; then
        balance_hex=$(echo "$response" | grep -o '"result":"[^"]*"' | cut -d'"' -f4)
    fi
    
    # 检查是否成功提取
    if [ -z "$balance_hex" ] || [ "$balance_hex" = "null" ]; then
        echo -e "${RED}错误: 无法获取余额${NC}" >&2
        echo "响应内容: $response" >&2
        return 1
    fi
    
    echo "$balance_hex"
}

# 主函数
main() {
    # 检查参数
    if [ $# -eq 0 ] || [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
        show_help
        exit 0
    fi
    
    ADDRESS="$1"
    if [ $# -ge 2 ]; then
        RPC_URL="$2"
    fi
    
    # 检查依赖和验证地址
    check_dependencies
    validate_address "$ADDRESS"
    
    echo "查询地址: $ADDRESS"
    echo "RPC节点:  $RPC_URL"
    echo ""
    echo -e "${YELLOW}正在查询余额...${NC}"
    
    # 获取余额
    balance_hex=$(get_balance "$ADDRESS" "$RPC_URL")
    
    # 检查是否获取成功
    if [ $? -ne 0 ] || [ -z "$balance_hex" ]; then
        echo -e "${RED}查询失败${NC}"
        exit 1
    fi
    
    # 转换余额
    balance_wei=$(hex_to_dec "$balance_hex")
    balance_eth=$(wei_to_eth "$balance_wei")
    formatted_eth=$(format_eth "$balance_eth")
    
    # 显示结果
    echo ""
    echo -e "${GREEN}余额: $formatted_eth ETH${NC}"
    echo "Wei: $balance_wei"
    echo "Hex: $balance_hex"
}

# 运行主函数
main "$@"
