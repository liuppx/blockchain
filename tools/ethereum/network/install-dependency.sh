#!/bin/bash

set -e

# 检测系统
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
PRYSM_VERSION="v6.0.3"

case $ARCH in
x86_64) ARCH="amd64" ;;
aarch64 | arm64) ARCH="arm64" ;;
armv7l) ARCH="arm" ;;
*)
    echo "❌ Unsupported architecture: $ARCH"
    return 1
    ;;
esac

mkdir -p ./dist
printf "\033[32m[INFO]\033[0m Setting up Ethereum PoS Private Network on macOS...\n"

# 检查是否为 macOS
if [[ "$(uname)" != "Darwin" ]]; then
    printf "\033[31m[ERROR]\033[0m This script is designed for macOS only\n"
    exit 1
fi

# 安装 Geth (Go Ethereum)
install_geth() {
    printf "\033[32m[INFO]\033[0m Installing Geth...\n"

    if command -v geth &>/dev/null; then
        printf "\033[33m[WARN]\033[0m Geth already installed: $(geth version | grep Version)\n"
        return
    fi

    # 从源码编译最新版本
    cd /tmp
    git clone https://github.com/ethereum/go-ethereum.git
    cd go-ethereum
    make geth
    printf "\033[32m[SUCCESS]\033[0m Geth installed: $(build/bin/geth version | head -n1)\n"

    # 复制到系统路径, 请把这个路径<parent directory>build/bin添加到PATH环境变量中
}

# 安装 beacon-chain
install_beacon_chain() {
    printf "\033[32m[INFO]\033[0m Installing beacon-chain binary...\n"
    name=beacon-chain-${PRYSM_VERSION}-${OS}-${ARCH}
    if [[ -f "dist/${name}" ]]; then
        printf "\033[33m[WARN]\033[0m Beacon-chain installed: ${PRYSM_VERSION}\n"
        cp -f dist/${name} beacon-chain
        chmod +x beacon-chain
        return
    fi

    # 下载二进制文件
    BASE_URL="https://github.com/prysmaticlabs/prysm/releases/download/${PRYSM_VERSION}"
    echo "📦 Downloading beacon-chain..."
    if curl -L "${BASE_URL}/${name}" -o dist/${name}; then
        cp -f dist/${name} beacon-chain
        chmod +x beacon-chain
        echo "✅ beacon-chain downloaded"
    else
        echo "❌ Failed to download beacon-chain"
        return 1
    fi
}

install_validator() {
    printf "\033[32m[INFO]\033[0m Installing validator binary...\n"
    name=validator-${PRYSM_VERSION}-${OS}-${ARCH}
    if [[ -f "dist/${name}" ]]; then
        printf "\033[33m[WARN]\033[0m Validator installed: ${PRYSM_VERSION}\n"
        cp -f dist/${name} validator
        chmod +x validator
        return
    fi

    # 下载二进制文件
    BASE_URL="https://github.com/prysmaticlabs/prysm/releases/download/${PRYSM_VERSION}"
    echo "📦 Downloading validator..."
    if curl -L "${BASE_URL}/${name}" -o dist/${name}; then
        cp -f dist/${name} validator
        chmod +x validator
        echo "✅ validator downloaded"
    else
        echo "❌ Failed to download validator"
        return 1
    fi
}

# 单独安装 prysmctl 二进制文件
install_prysmctl() {
    printf "\033[32m[INFO]\033[0m Installing prysmctl binary...\n"
    name=prysmctl-${PRYSM_VERSION}-${OS}-${ARCH}
    if [[ -f "dist/${name}" ]]; then
        printf "\033[33m[WARN]\033[0m Prysmctl installed: ${PRYSM_VERSION}\n"
        cp -vf dist/${name} prysmctl
        chmod +x prysmctl
        return
    fi


    BASE_URL="https://github.com/prysmaticlabs/prysm/releases/download/${PRYSM_VERSION}"
    # local download_url="https://github.com/OffchainLabs/prysm/releases/download/${version}/${binary_name}"
    echo "📦 Downloading prysmctl..."
    if curl -L "${BASE_URL}/${name}" -o dist/${name}; then
        cp -f dist/${name} prysmctl
        chmod +x prysmctl
        echo "✅ prysmctl downloaded"
    else
        echo "❌ Failed to download prysmctl"
        return 1
    fi
}

# 安装 staking-deposit-cli
install_staking_deposit_cli() {
    printf "\033[32m[INFO]\033[0m Installing staking-deposit-cli from source...\n"

    # 检查是否已存在
    if [[ -d "staking-deposit-cli" ]]; then
        printf "\033[33m[WARN]\033[0m staking-deposit-cli installed: $(cd staking-deposit-cli && git describe --tags)\n"
        return
    fi

    # 克隆最新源代码
    printf "\033[32m[INFO]\033[0m Cloning staking-deposit-cli repository...\n"
    git clone https://github.com/ethereum/staking-deposit-cli.git
    cd staking-deposit-cli

    # 检出最新稳定版本
    LATEST_TAG=$(git describe --tags --abbrev=0)
    printf "\033[32m[INFO]\033[0m Checking out latest version: $LATEST_TAG\n"
    git checkout $LATEST_TAG

    # 创建虚拟环境
    printf "\033[32m[INFO]\033[0m Creating Python virtual environment...\n"
    python3 -m venv venv
    source venv/bin/activate

    # 升级 pip 和安装构建依赖
    printf "\033[32m[INFO]\033[0m Installing build dependencies...\n"
    pip install --upgrade pip setuptools wheel

    # 安装项目依赖
    printf "\033[32m[INFO]\033[0m Installing project dependencies...\n"
    pip install -r requirements.txt

    # 安装开发依赖（如果需要）
    if [[ -f "requirements_test.txt" ]]; then
        pip install -r requirements_test.txt
    fi

    # 以开发模式安装
    printf "\033[32m[INFO]\033[0m Installing staking-deposit-cli in development mode...\n"
    pip install -e .

    # 验证安装
    printf "\033[32m[INFO]\033[0m Verifying installation...\n"
    python -m staking_deposit.deposit --help >/dev/null 2>&1

    if [[ $? -eq 0 ]]; then
        printf "\033[32m[SUCCESS]\033[0m staking-deposit-cli installed successfully from source\n"
        printf "\033[32m[INFO]\033[0m Version: $LATEST_TAG\n"
    else
        printf "\033[31m[ERROR]\033[0m Failed to verify staking-deposit-cli installation\n"
        exit 1
    fi

    # 返回上级目录
    cd ..
}

# 主安装流程
main() {
    printf "\033[32m=== Ethereum PoS Private Network Setup for macOS ===\033[0m\n"

    install_geth
    install_beacon_chain
    install_validator
    install_prysmctl
    install_staking_deposit_cli

    printf "\033[32m[SUCCESS]\033[0m All dependencies installed successfully!\n"
    printf "\n"
    printf "Next steps:\n"
    printf "1. Run: ./setup-config.sh\n"
    printf "2. Run: ./start-network.sh\n"
}

main "$@"
