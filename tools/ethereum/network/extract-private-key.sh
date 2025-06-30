#!/bin/bash

# extract-private-key.sh - 提取以太坊账户私钥 (支持虚拟环境)

# 默认配置
KEYSTORE_DIR="accounts"
PASSWORD_FILE="config/password.txt"
VENV_DIR="/tmp/.venv"

# 颜色定义
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# 显示帮助信息
show_help() {
    echo "用法: $0 [选项]"
    echo ""
    echo "选项:"
    echo "  -k, --keystore DIR    keystore目录 (默认: accounts)"
    echo "  -p, --password FILE   密码文件 (默认: config/password.txt)"
    echo "  -f, --file FILE       直接指定keystore文件"
    echo "  -v, --venv DIR        虚拟环境目录 (默认: /tmp/.venv)"
    echo "  --no-venv            不使用虚拟环境"
    echo "  -h, --help           显示帮助信息"
    echo ""
    echo "示例:"
    echo "  $0                                    # 使用默认配置和虚拟环境"
    echo "  $0 --no-venv                          # 不使用虚拟环境"
    echo "  $0 -v ./my-venv                       # 指定虚拟环境目录"
    echo "  $0 -k ./my-accounts -p ./my-pass.txt  # 指定目录和密码文件"
    echo "  $0 -f accounts/UTC--xxx--xxx          # 直接指定keystore文件"
}

# 设置和激活虚拟环境
setup_venv() {
    local venv_dir="$1"
    local use_venv="$2"

    if [ "$use_venv" = "false" ]; then
        echo -e "${YELLOW}跳过虚拟环境设置${NC}"
        PYTHON_CMD="python3"
        PIP_CMD="pip3"
        return 0
    fi

    echo -e "${BLUE}设置Python虚拟环境...${NC}"

    # 检查python3是否可用
    if ! command -v python3 &>/dev/null; then
        echo -e "${RED}错误: 需要安装 python3${NC}"
        exit 1
    fi

    # 创建虚拟环境（如果不存在）
    if [ ! -d "$venv_dir" ]; then
        echo "创建虚拟环境: $venv_dir"
        python3 -m venv "$venv_dir"
        if [ $? -ne 0 ]; then
            echo -e "${RED}错误: 无法创建虚拟环境${NC}"
            echo "请确保安装了 python3-venv: sudo apt install python3-venv"
            exit 1
        fi
    fi

    # 激活虚拟环境
    source "$venv_dir/bin/activate"
    if [ $? -ne 0 ]; then
        echo -e "${RED}错误: 无法激活虚拟环境${NC}"
        exit 1
    fi

    echo -e "${GREEN}✓ 虚拟环境已激活: $venv_dir${NC}"

    # 设置命令
    PYTHON_CMD="python"
    PIP_CMD="pip"

    # 升级pip
    echo "升级pip..."
    $PIP_CMD install --upgrade pip >/dev/null 2>&1
}

# 检查和安装依赖
check_dependencies() {
    local use_venv="$1"

    echo -e "${BLUE}检查依赖...${NC}"

    # 检查eth-account是否安装
    if ! $PYTHON_CMD -c "import eth_account" 2>/dev/null; then
        echo -e "${YELLOW}正在安装 eth-account...${NC}"
        $PIP_CMD install eth-account
        if [ $? -ne 0 ]; then
            echo -e "${RED}错误: 无法安装 eth-account${NC}"
            if [ "$use_venv" = "true" ]; then
                echo "请检查虚拟环境设置"
            else
                echo "请手动安装: pip3 install eth-account"
            fi
            exit 1
        fi
        echo -e "${GREEN}✓ eth-account 安装成功${NC}"
    else
        echo -e "${GREEN}✓ eth-account 已安装${NC}"
    fi
}

# 清理虚拟环境
cleanup_venv() {
    local use_venv="$1"

    if [ "$use_venv" = "true" ] && [ -n "$VIRTUAL_ENV" ]; then
        echo -e "${BLUE}退出虚拟环境${NC}"
        deactivate 2>/dev/null || true
    fi
}

# 列出keystore文件
list_keystore_files() {
    local dir="$1"

    if [ ! -d "$dir" ]; then
        echo -e "${RED}错误: keystore目录不存在: $dir${NC}" >&2
        exit 1
    fi

    # 使用更兼容的方式获取文件列表
    local files=()
    while IFS= read -r -d '' file; do
        files+=("$file")
    done < <(find "$dir" -name "UTC--*" -type f -print0 2>/dev/null)

    if [ ${#files[@]} -eq 0 ]; then
        echo -e "${RED}错误: 在 $dir 中没有找到keystore文件${NC}" >&2
        exit 1
    fi

    # 将所有显示信息输出到stderr
    echo -e "${BLUE}找到的keystore文件:${NC}" >&2
    for i in "${!files[@]}"; do
        local filename=$(basename "${files[$i]}")
        local address=$(echo "$filename" | grep -o '[0-9a-fA-F]\{40\}$')
        echo "  [$((i + 1))] $filename" >&2
        if [ -n "$address" ]; then
            echo "      地址: 0x$address" >&2
        fi
    done

    echo "" >&2

    local selection=""
    while [ -z "$selection" ]; do
        read -p "请选择文件编号 [1-${#files[@]}]: " selection >&2

        if [[ ! "$selection" =~ ^[0-9]+$ ]] || [ "$selection" -lt 1 ] || [ "$selection" -gt ${#files[@]} ]; then
            echo -e "${RED}错误: 无效的选择，请重新输入${NC}" >&2
            selection=""
        fi
    done

    # 只输出选中的文件路径到stdout
    echo "${files[$((selection - 1))]}"
}

# 提取私钥
extract_private_key() {
    local keystore_file="$1"
    local password_file="$2"
    echo "$1---"
    echo "$2---"
    # 检查文件是否存在
    if [ ! -f "$keystore_file" ]; then
        echo -e "${RED}错误: keystore文件不存在: $keystore_file${NC}"
        exit 1
    fi

    if [ ! -f "$password_file" ]; then
        echo -e "${RED}错误: 密码文件不存在: $password_file${NC}"
        exit 1
    fi

    # 创建临时Python脚本
    local temp_script=$(mktemp /tmp/extract_key_XXXXXX.py)

    cat >"$temp_script" <<'EOF'
import json
import sys
from eth_account import Account

def extract_key(keystore_path, password_path):
    try:
        # 读取密码
        with open(password_path, 'r') as f:
            password = f.read().strip()
        
        # 读取keystore
        with open(keystore_path, 'r') as f:
            keystore = json.load(f)
        
        # 解密私钥
        private_key = Account.decrypt(keystore, password)
        account = Account.from_key(private_key)
        
        print(f"地址: {account.address}")
        print(f"私钥: 0x{private_key.hex()}")
        
        return True
        
    except FileNotFoundError as e:
        print(f"文件不存在: {e}")
        return False
    except json.JSONDecodeError:
        print("keystore文件格式错误")
        return False
    except ValueError as e:
        if "MAC mismatch" in str(e):
            print("密码错误")
        else:
            print(f"解密失败: {e}")
        return False
    except Exception as e:
        print(f"未知错误: {e}")
        return False

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print("用法: python script.py <keystore_file> <password_file>")
        sys.exit(1)
    
    keystore_path = sys.argv[1]
    password_path = sys.argv[2]
    
    success = extract_key(keystore_path, password_path)
    sys.exit(0 if success else 1)
EOF

    # 执行Python脚本
    echo -e "${YELLOW}正在提取私钥...${NC}"
    echo ""

    $PYTHON_CMD "$temp_script" "$keystore_file" "$password_file"
    local result=$?

    # 清理临时文件
    rm -f "$temp_script"

    if [ $result -eq 0 ]; then
        echo ""
        echo -e "${GREEN}✓ 私钥提取成功${NC}"
        echo -e "${RED}⚠️  请妥善保管私钥，不要泄露给他人！${NC}"
    else
        echo ""
        echo -e "${RED}✗ 私钥提取失败${NC}"
        exit 1
    fi
}

# 主函数
main() {
    local DIRECT_FILE=""
    local use_venv="true"

    # 解析参数
    while [[ $# -gt 0 ]]; do
        case $1 in
        -k | --keystore)
            KEYSTORE_DIR="$2"
            shift 2
            ;;
        -p | --password)
            PASSWORD_FILE="$2"
            shift 2
            ;;
        -f | --file)
            DIRECT_FILE="$2"
            shift 2
            ;;
        -v | --venv)
            VENV_DIR="$2"
            shift 2
            ;;
        --no-venv)
            use_venv="false"
            shift
            ;;
        -h | --help)
            show_help
            exit 0
            ;;
        *)
            echo -e "${RED}错误: 未知参数 $1${NC}"
            show_help
            exit 1
            ;;
        esac
    done

    # 设置信号处理，确保退出时清理虚拟环境
    # trap "cleanup_venv $use_venv" EXIT

    # 设置虚拟环境
    setup_venv "$VENV_DIR" "$use_venv"

    # 检查依赖
    check_dependencies "$use_venv"

    # 确定keystore文件
    if [ ! -n "$DIRECT_FILE" ]; then
        DIRECT_FILE=$(list_keystore_files "$KEYSTORE_DIR")
    fi

    echo ""
    echo "Keystore文件: $DIRECT_FILE"
    echo "密码文件: $PASSWORD_FILE"
    if [ "$use_venv" = "true" ]; then
        echo "虚拟环境: $VENV_DIR"
    fi
    echo ""

    # 确认操作
    read -p "确认提取私钥? [y/N]: " confirm
    if [[ ! "$confirm" =~ ^[Yy]$ ]]; then
        echo "操作已取消"
        exit 0
    fi

    # 提取私钥
    extract_private_key "$DIRECT_FILE" "$PASSWORD_FILE"
}

# 运行主函数
main "$@"
