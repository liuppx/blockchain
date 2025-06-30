#!/bin/bash

set -e

# 导入通用配置
source common.sh

# 全局变量存储创建的账户地址
RECIPIENT_ADDRESS=""
USER_ADDRESS=""

# 生成 JWT 密钥
generate_jwt_secret() {
    log_info "Generating JWT secret..."
    openssl rand -hex 32 >$OUTPUT_DIR/config/jwt.hex
}

# 创建账户
create_accounts() {
    # 创建密码文件
    echo "${PASSWORD}" >$OUTPUT_DIR/config/password.txt
    
    # 创建 User 账户
    log_info "Creating user account..."
    USER_ADDRESS=$(geth account new --password $OUTPUT_DIR/config/password.txt --keystore $OUTPUT_DIR/accounts/ 2>/dev/null | grep -o '0x[a-fA-F0-9]\{40\}')
    if [[ -n "$USER_ADDRESS" ]]; then
        echo "$USER_ADDRESS" >$OUTPUT_DIR/config/user_address.txt
        log_info "User account created: $USER_ADDRESS"
    else
        log_error "Failed to create user account"
        exit 1
    fi

    # 创建 Recipient 账户
    log_info "Creating recipient account..."
    RECIPIENT_ADDRESS=$(geth account new --password $OUTPUT_DIR/config/password.txt --keystore $OUTPUT_DIR/accounts/ 2>/dev/null | grep -o '0x[a-fA-F0-9]\{40\}')
    if [[ -n "$RECIPIENT_ADDRESS" ]]; then
        echo "$RECIPIENT_ADDRESS" >$OUTPUT_DIR/config/recipient_address.txt
        log_info "Recipient account created: $RECIPIENT_ADDRESS"
    else
        log_error "Failed to create recipient account"
        exit 1
    fi

    # 创建 Withdrawal 账户
    log_info "Creating Withdrawal account..."
    WITHDRAWAL_ADDRESS=$(geth account new --password $OUTPUT_DIR/config/password.txt --keystore $OUTPUT_DIR/accounts/ 2>/dev/null | grep -o '0x[a-fA-F0-9]\{40\}')
    if [[ -n "$WITHDRAWAL_ADDRESS" ]]; then
        echo "$WITHDRAWAL_ADDRESS" >$OUTPUT_DIR/config/withdrawal_address.txt
        log_info "Withdrawal account created: $WITHDRAWAL_ADDRESS"
    else
        log_error "Failed to create withdrawal account"
        exit 1
    fi
}

# 生成验证者助记词
generate_mnemonics() {
    log_info "Generating mnemonics..."
    eth2-val-tools mnemonic > $OUTPUT_DIR/config/mnemonics.txt
    log_info "Mnemonics file created."
}

# 主函数
main() {
    log_header "=== Ethereum PoS Private Network Basic Setup ==="
    
    clean_data
    create_directories
    create_accounts
    generate_jwt_secret
    generate_mnemonics

    log_info "✅ Basic configuration setup completed!"
    log_warn "[NEXT] Run individual service setup scripts or './start-network.sh' to start the network"
}

# 运行主函数
main "$@"
