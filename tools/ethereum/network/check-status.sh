#!/bin/bash

# check-status.sh
set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

printf "${CYAN}=== Network Status Check ===${NC}\n"

# 检查 Geth 状态
printf "\n${BLUE}[Geth Status]${NC}\n"
if curl -s -X POST -H "Content-Type: application/json" \
   --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
   http://localhost:8545 | jq '.result' 2>/dev/null; then
    printf "${GREEN}✓ Geth is responding${NC}\n"
    
    # 检查最新区块
    LATEST_BLOCK=$(curl -s -X POST -H "Content-Type: application/json" \
       --data '{"jsonrpc":"2.0","method":"eth_getBlockByNumber","params":["latest",false],"id":1}' \
       http://localhost:8545 | jq -r '.result.number')
    printf "Latest block: $LATEST_BLOCK\n"
    
    # 检查同步状态
    SYNC_STATUS=$(curl -s -X POST -H "Content-Type: application/json" \
       --data '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' \
       http://localhost:8545 | jq '.result')
    printf "Sync status: $SYNC_STATUS\n"
else
    printf "${RED}✗ Geth is not responding${NC}\n"
fi

# 检查 Beacon Chain 状态
printf "\n${BLUE}[Beacon Chain Status]${NC}\n"
if curl -s http://localhost:3500/eth/v1/node/health >/dev/null 2>&1; then
    printf "${GREEN}✓ Beacon Chain is responding${NC}\n"
    
    # 检查创世信息
    printf "Genesis info:\n"
    curl -s "http://localhost:3500/eth/v1/beacon/genesis" | jq '.' 2>/dev/null || printf "Failed to get genesis info\n"
    
    # 检查当前头部
    printf "\nHead info:\n"
    curl -s "http://localhost:3500/eth/v1/beacon/headers/head" | jq '.data.header.message | {slot, proposer_index}' 2>/dev/null || printf "Failed to get head info\n"
    
    # 检查验证者数量
    VALIDATOR_COUNT=$(curl -s "http://localhost:3500/eth/v1/beacon/states/head/validators" | jq '.data | length' 2>/dev/null)
    printf "Active validators: $VALIDATOR_COUNT\n"
    
    # 检查验证者状态
    printf "\nValidator statuses:\n"
    curl -s "http://localhost:3500/eth/v1/beacon/states/head/validators" | \
        jq -r '.data[] | "\(.index): \(.status)"' 2>/dev/null | head -10 || printf "Failed to get validator statuses\n"
else
    printf "${RED}✗ Beacon Chain is not responding${NC}\n"
fi

# 检查进程状态
printf "\n${BLUE}[Process Status]${NC}\n"
if [[ -f $OUTPUT_DIR/.geth.pid ]] && kill -0 $(cat $OUTPUT_DIR/.geth.pid) 2>/dev/null; then
    printf "${GREEN}✓ Geth process running (PID: $(cat $OUTPUT_DIR/.geth.pid))${NC}\n"
else
    printf "${RED}✗ Geth process not running${NC}\n"
fi

if [[ -f $OUTPUT_DIR/.beacon.pid ]] && kill -0 $(cat $OUTPUT_DIR/.beacon.pid) 2>/dev/null; then
    printf "${GREEN}✓ Beacon Chain process running (PID: $(cat $OUTPUT_DIR/.beacon.pid))${NC}\n"
else
    printf "${RED}✗ Beacon Chain process not running${NC}\n"
fi

if [[ -f $OUTPUT_DIR/.validator.pid ]] && kill -0 $(cat $OUTPUT_DIR/.validator.pid) 2>/dev/null; then
    printf "${GREEN}✓ Validator process running (PID: $(cat $OUTPUT_DIR/.validator.pid))${NC}\n"
else
    printf "${RED}✗ Validator process not running${NC}\n"
fi

# 检查日志中的错误
printf "\n${BLUE}[Recent Errors]${NC}\n"
if [[ -f $OUTPUT_DIR/logs/geth.log ]]; then
    printf "Geth errors (last 5):\n"
    grep -i "error\|fatal\|panic" $OUTPUT_DIR/logs/geth.log | tail -5 || printf "No recent errors in Geth log\n"
fi

if [[ -f $OUTPUT_DIR/logs/beacon.log ]]; then
    printf "\nBeacon Chain errors (last 5):\n"
    grep -i "error\|fatal\|panic" $OUTPUT_DIR/logs/beacon.log | tail -5 || printf "No recent errors in Beacon Chain log\n"
fi

if [[ -f $OUTPUT_DIR/logs/validator.log ]]; then
    printf "\nValidator errors (last 5):\n"
    grep -i "error\|fatal\|panic" $OUTPUT_DIR/logs/validator.log | tail -5 || printf "No recent errors in Validator log\n"
fi

printf "\n${CYAN}=== Status Check Complete ===${NC}\n"
